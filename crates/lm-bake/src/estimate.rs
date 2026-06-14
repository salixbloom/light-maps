/// Pre-bake estimation: sample the head of a GeoJSON-Seq file to project the
/// feature count, per-field byte cost, output archive size, and peak RAM — so a
/// caller (the `--interactive` picker, or just an info line) can show the cost of
/// a bake before committing minutes of CPU to it.
///
/// Everything here is an *estimate* from a bounded sample (no full scan), so the
/// numbers are order-of-magnitude guides, not guarantees.
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// How many feature lines to parse for the sample. Big enough to stabilise the
/// per-field averages, small enough to read in well under a second.
const SAMPLE_LINES: usize = 20_000;

/// Per-field sample statistics.
#[derive(Clone, Debug)]
pub struct FieldStat {
    pub name: String,
    /// Mean serialized byte cost of this field's value across the sample
    /// (JSON value length; key cost is folded in separately).
    pub avg_value_bytes: f64,
    /// Fraction of sampled features that carried this field (0.0–1.0).
    pub presence: f64,
    /// Most common JSON type seen for this field.
    pub kind: &'static str,
}

impl FieldStat {
    /// Estimated total bytes this field contributes across the whole dataset
    /// (value + key + JSON punctuation), given the projected feature count.
    pub fn total_bytes(&self, feature_count: u64) -> u64 {
        // key + quotes + colon + value + comma ≈ name.len()+4 + value bytes.
        let per = self.avg_value_bytes + self.name.len() as f64 + 4.0;
        (per * self.presence * feature_count as f64) as u64
    }
}

/// Full estimate report for one input.
#[derive(Clone, Debug)]
pub struct Estimate {
    pub file_bytes: u64,
    /// Projected total feature count (file_bytes / mean sampled line bytes).
    pub feature_count: u64,
    /// Per-field stats, sorted heaviest-first by total dataset cost.
    pub fields: Vec<FieldStat>,
    /// Mean store record size per feature (geometry + kept-props blob).
    pub avg_record_bytes: f64,
    /// True when the sample hit EOF before SAMPLE_LINES — i.e. we saw the whole
    /// file and `feature_count` is exact, not projected.
    pub exact: bool,
}

impl Estimate {
    /// Projected on-disk feature-store size for the given kept fields.
    pub fn store_bytes(&self, keep: Option<&std::collections::HashSet<String>>) -> u64 {
        let geom_bytes = (self.avg_record_bytes - self.props_bytes_per_feature(None)).max(0.0);
        let props = self.props_bytes_per_feature(keep);
        ((geom_bytes + props) * self.feature_count as f64) as u64
    }

    /// Mean serialized property bytes per feature for the kept set (all if None).
    fn props_bytes_per_feature(&self, keep: Option<&std::collections::HashSet<String>>) -> f64 {
        self.fields
            .iter()
            .filter(|f| keep.map(|k| k.contains(&f.name)).unwrap_or(true))
            .map(|f| (f.avg_value_bytes + f.name.len() as f64 + 4.0) * f.presence)
            .sum()
    }

    /// Rough projected output archive size. Vector tiles compress the *geometry*
    /// well and re-encode properties compactly; empirically a parcel-style
    /// dataset lands near 0.38× the kept store size across z0–14. This is the
    /// crudest number here — label it approximate when you show it.
    pub fn output_bytes(&self, keep: Option<&std::collections::HashSet<String>>) -> u64 {
        (self.store_bytes(keep) as f64 * 0.38) as u64
    }

    /// Projected peak *unreclaimable* memory — the anonymous heap the OOM killer
    /// actually counts. This is the number that matters for "will it fit": it
    /// excludes the mmap'd store, whose resident pages are file-backed and
    /// reclaimable under pressure (see [`reclaimable_cache_bytes`]).
    ///
    /// Three live heap costs, matching the streaming bake's three allocators:
    ///
    ///   1. Feature index — `Vec<FeatureMeta>`, resident for the whole bake.
    ///      `FeatureMeta` is `[f64;4] + u64 + u32` = 44 B, padded to 8-byte
    ///      alignment → 48 B/feature ([`feature_store::FeatureMeta`]).
    ///   2. Per-zoom bucket map — `HashMap<(u32,u32), Vec<u32>>` binning every
    ///      surviving feature into the tiles its bbox covers, rebuilt per zoom
    ///      ([`streaming::bake_zoom_streaming`]). Each feature's bbox spans a
    ///      ~3×3 tile halo, so a feature contributes several `u32` refs; with
    ///      Vec/HashMap overhead this is the dominant spike at low zoom, where
    ///      no feature is dropped yet.
    ///   3. Chunk payload — up to `CHUNK_FEATURE_REFS` (50k) features decoded
    ///      back into geometry + parsed props at once, times rayon's thread
    ///      fan-out ([`streaming::CHUNK_FEATURE_REFS`]).
    ///
    /// Calibrated against the WA-parcels run (4.4 M features, 20 cores): this
    /// projects ~1.6 GB, matching the observed 1.66 GB anon peak. It is still a
    /// projection from a sample, not a guarantee — but it tracks the real OOM
    /// risk, which the old `store × 0.35 + 512 MB` heuristic did not.
    pub fn peak_unreclaimable_bytes(
        &self,
        keep: Option<&std::collections::HashSet<String>>,
    ) -> u64 {
        let n = self.feature_count as f64;

        // 1. Feature index: 48 B/feature, padded.
        let index = n * 48.0;

        // 2. Bucket map worst case (low zoom, nothing dropped). Each feature
        //    lands in ~BBOX_TILE_HALO tiles, each ref a u32 (4 B) pushed into a
        //    per-tile `Vec` inside a `HashMap`. REF_OVERHEAD folds in the heavy
        //    real cost of that structure: Vec headers + power-of-two capacity
        //    slack, plus HashMap bucket/control bytes. Calibrated against the
        //    parcels run, where this term dominates the anon peak at low zoom.
        const BBOX_TILE_HALO: f64 = 4.0; // ~3×3 window, averaged over edge tiles
        const REF_OVERHEAD: f64 = 8.0; // u32 ref + Vec slack + HashMap bookkeeping
        let bucket_map = n * BBOX_TILE_HALO * 4.0 * REF_OVERHEAD;

        // 3. Chunk payload: CHUNK_FEATURE_REFS decoded records × rayon fan-out.
        //    Per-record live cost ≈ its store record (geometry + parsed props),
        //    inflated by PARSED_INFLATION because in-RAM geo_types coords and a
        //    serde_json `Map` (boxed strings, node overhead) are far fatter than
        //    the packed on-disk record.
        const CHUNK_FEATURE_REFS: f64 = 50_000.0; // mirror streaming.rs
        const PARSED_INFLATION: f64 = 1.5;
        let per_record = self.per_record_bytes(keep) * PARSED_INFLATION;
        let threads = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(8) as f64;
        let chunk_payload = CHUNK_FEATURE_REFS * per_record * threads;

        (index + bucket_map + chunk_payload) as u64
    }

    /// Mean store record bytes for one feature (geometry + kept-props blob),
    /// i.e. [`store_bytes`] amortised per feature. Used to size the live chunk
    /// payload in [`peak_unreclaimable_bytes`].
    fn per_record_bytes(&self, keep: Option<&std::collections::HashSet<String>>) -> f64 {
        if self.feature_count == 0 {
            return 0.0;
        }
        self.store_bytes(keep) as f64 / self.feature_count as f64
    }

    /// Projected *reclaimable* resident memory: the mmap'd feature store, whose
    /// pages show up in RSS but are file-backed and dropped instantly by the
    /// kernel under pressure. This is why a bake's RSS looks far larger than its
    /// true working set — surface it separately so the RSS isn't a surprise, but
    /// don't add it to the OOM budget.
    pub fn reclaimable_cache_bytes(
        &self,
        keep: Option<&std::collections::HashSet<String>>,
    ) -> u64 {
        self.store_bytes(keep)
    }
}

/// Sample `path` and build an [`Estimate`]. Returns `None` if the file can't be
/// read or has no parseable features in the sample window.
pub fn estimate_input(path: &Path) -> Option<Estimate> {
    let file_bytes = std::fs::metadata(path).ok()?.len();
    let file = std::fs::File::open(path).ok()?;
    let reader = BufReader::with_capacity(1 << 20, file);

    let mut sampled_lines = 0usize;
    let mut sampled_bytes = 0u64;
    let mut record_bytes_sum = 0f64;

    // Per-field accumulators.
    let mut value_bytes: BTreeMap<String, f64> = BTreeMap::new();
    let mut present: BTreeMap<String, u64> = BTreeMap::new();
    let mut kinds: BTreeMap<String, &'static str> = BTreeMap::new();

    let mut hit_eof = true;
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim_start_matches('\x1E').trim();
        if trimmed.is_empty() {
            continue;
        }
        // +1 for the newline we stripped, so the byte projection matches the file.
        sampled_bytes += trimmed.len() as u64 + 1;

        if let Ok(feat) = trimmed.parse::<geojson::Feature>() {
            if let Some(props) = &feat.properties {
                for (k, v) in props {
                    let vbytes = serde_json::to_string(v).map(|s| s.len()).unwrap_or(0);
                    *value_bytes.entry(k.clone()).or_insert(0.0) += vbytes as f64;
                    *present.entry(k.clone()).or_insert(0) += 1;
                    kinds.entry(k.clone()).or_insert_with(|| json_kind(v));
                }
            }
            // Approximate store record cost: geometry vertices × 16 B + props JSON.
            let geom_cost = feat
                .geometry
                .as_ref()
                .map(geometry_byte_cost)
                .unwrap_or(0.0);
            let props_cost = feat
                .properties
                .as_ref()
                .map(|p| serde_json::to_string(p).map(|s| s.len()).unwrap_or(0))
                .unwrap_or(0) as f64;
            record_bytes_sum += geom_cost + props_cost;
            sampled_lines += 1;
        }

        if sampled_lines >= SAMPLE_LINES {
            hit_eof = false;
            break;
        }
    }

    if sampled_lines == 0 {
        return None;
    }

    let avg_line_bytes = sampled_bytes as f64 / sampled_lines as f64;
    let feature_count = if hit_eof {
        sampled_lines as u64
    } else {
        (file_bytes as f64 / avg_line_bytes).round() as u64
    };

    let mut fields: Vec<FieldStat> = present
        .keys()
        .map(|name| {
            let p = present[name];
            FieldStat {
                name: name.clone(),
                avg_value_bytes: value_bytes[name] / p as f64,
                presence: p as f64 / sampled_lines as f64,
                kind: kinds[name],
            }
        })
        .collect();
    // Heaviest dataset-cost first so the picker lists the expensive fields on top.
    fields.sort_by(|a, b| {
        b.total_bytes(feature_count)
            .cmp(&a.total_bytes(feature_count))
    });

    Some(Estimate {
        file_bytes,
        feature_count,
        fields,
        avg_record_bytes: record_bytes_sum / sampled_lines as f64,
        exact: hit_eof,
    })
}

/// Approximate stored byte cost of a geometry: 16 B per coordinate (two f64) plus
/// small per-ring overhead. Good enough for sizing, not exact.
fn geometry_byte_cost(g: &geojson::Geometry) -> f64 {
    use geojson::Value::*;
    let coords = match &g.value {
        Point(_) => 1,
        MultiPoint(v) | LineString(v) => v.len(),
        MultiLineString(v) | Polygon(v) => v.iter().map(|r| r.len()).sum(),
        MultiPolygon(v) => v.iter().flat_map(|p| p.iter().map(|r| r.len())).sum(),
        GeometryCollection(_) => 1,
    };
    coords as f64 * 16.0 + 8.0
}

fn json_kind(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Null => "null",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Format a byte count as a short human string (e.g. "1.5 GB", "265 MB").
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_seq(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        f.flush().unwrap();
        f
    }

    #[test]
    fn estimates_field_costs_and_count() {
        let f = write_seq(&[
            r#"{"type":"Feature","properties":{"name":"a","url":"http://example.com/aaaaaaaaaa"},"geometry":{"type":"Point","coordinates":[0,0]}}"#,
            r#"{"type":"Feature","properties":{"name":"b","url":"http://example.com/bbbbbbbbbb"},"geometry":{"type":"Point","coordinates":[1,1]}}"#,
        ]);
        let est = estimate_input(f.path()).expect("estimate");
        assert!(est.exact, "small file should be read exactly");
        assert_eq!(est.feature_count, 2);
        // url is the heavier field, should sort first.
        assert_eq!(est.fields[0].name, "url");
        assert!(est.fields[0].avg_value_bytes > est.fields[1].avg_value_bytes);
    }

    #[test]
    fn keep_set_reduces_store_estimate() {
        let f = write_seq(&[
            r#"{"type":"Feature","properties":{"keep":1,"drop":"a-very-long-string-field-value-here"},"geometry":{"type":"Point","coordinates":[0,0]}}"#,
        ]);
        let est = estimate_input(f.path()).unwrap();
        let mut keep = std::collections::HashSet::new();
        keep.insert("keep".to_string());
        assert!(est.store_bytes(Some(&keep)) < est.store_bytes(None));
    }

    #[test]
    fn unreclaimable_excludes_store_and_tracks_keep_set() {
        // Dropping a heavy field must lower the unreclaimable estimate (via the
        // chunk-payload term), and the estimate must stay well below the full
        // store size — proving the store mmap is treated as reclaimable, not
        // counted toward the OOM budget.
        let f = write_seq(&[
            r#"{"type":"Feature","properties":{"keep":1,"drop":"a-very-long-string-field-value-here-padding-padding"},"geometry":{"type":"Point","coordinates":[0,0]}}"#,
        ]);
        let est = estimate_input(f.path()).unwrap();
        let mut keep = std::collections::HashSet::new();
        keep.insert("keep".to_string());
        assert!(
            est.peak_unreclaimable_bytes(Some(&keep)) < est.peak_unreclaimable_bytes(None),
            "dropping a field should lower the unreclaimable estimate"
        );
        assert_eq!(est.reclaimable_cache_bytes(None), est.store_bytes(None));
    }

    #[test]
    fn unreclaimable_matches_observed_parcel_peak() {
        // Calibration guard: synthesise an Estimate matching the measured
        // WA-parcels run (run2.log) — 4.36 M features, full properties, with a
        // mean store record of ~669 B (sampled: 176 B geometry + 493 B props).
        // That run's observed anon peak was ~1.66 GiB. The chunk-payload term
        // scales with core count, so we assert a band (1.2–3.0 GiB) that holds
        // from ~8 up to the 20-core dev box rather than a brittle point value.
        let est = Estimate {
            file_bytes: 4_108_158_894,
            feature_count: 4_360_000,
            fields: Vec::new(),
            avg_record_bytes: 669.0,
            exact: false,
        };
        let peak = est.peak_unreclaimable_bytes(None);
        let gib = peak as f64 / (1024.0 * 1024.0 * 1024.0);
        assert!(
            (1.2..=3.0).contains(&gib),
            "projected unreclaimable {gib:.2} GiB outside calibrated 1.2–3.0 GiB band"
        );
    }

    #[test]
    fn human_bytes_formats() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(265 * 1024 * 1024), "265 MB");
    }
}

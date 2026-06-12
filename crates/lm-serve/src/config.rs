/// Runtime configuration parsed from CLI flags.
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ServeConfig {
    pub addr: String,
    pub base_url: Option<String>,
    /// Optional bearer / API key. If set, every request must include
    /// `Authorization: Bearer <key>`.
    pub api_key: Option<String>,
    /// CORS allowed origins. Empty = deny all cross-origin requests.
    /// "*" = allow all.
    pub cors_origins: Vec<String>,
    /// Per-request timeout.
    pub request_timeout: Duration,
    /// Maximum concurrent in-flight requests (backpressure).
    pub max_in_flight: usize,
    /// Maximum z coordinate accepted (z > this → 400).
    pub max_zoom_request: u8,
    /// Whether to expose /metrics.
    pub metrics_enabled: bool,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            addr: "0.0.0.0:3000".to_owned(),
            base_url: None,
            api_key: None,
            cors_origins: vec![],
            request_timeout: Duration::from_secs(10),
            max_in_flight: 512,
            max_zoom_request: 24,
            metrics_enabled: false,
        }
    }
}

impl ServeConfig {
    pub fn from_args(mut args: impl Iterator<Item = String>) -> anyhow::Result<(Vec<std::path::PathBuf>, Self)> {
        let mut paths = Vec::new();
        let mut cfg = ServeConfig::default();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--addr" => cfg.addr = next_val(&mut args, "--addr")?,
                "--base-url" => cfg.base_url = Some(next_val(&mut args, "--base-url")?),
                "--api-key" => cfg.api_key = Some(next_val(&mut args, "--api-key")?),
                "--cors" => cfg.cors_origins.push(next_val(&mut args, "--cors")?),
                "--timeout" => {
                    let secs: u64 = next_val(&mut args, "--timeout")?.parse()
                        .map_err(|_| anyhow::anyhow!("--timeout must be a number of seconds"))?;
                    cfg.request_timeout = Duration::from_secs(secs);
                }
                "--max-in-flight" => {
                    cfg.max_in_flight = next_val(&mut args, "--max-in-flight")?.parse()
                        .map_err(|_| anyhow::anyhow!("--max-in-flight must be an integer"))?;
                }
                "--max-zoom" => {
                    cfg.max_zoom_request = next_val(&mut args, "--max-zoom")?.parse()
                        .map_err(|_| anyhow::anyhow!("--max-zoom must be 0-30"))?;
                }
                "--metrics" => cfg.metrics_enabled = true,
                other => paths.push(std::path::PathBuf::from(other)),
            }
        }

        if paths.is_empty() {
            anyhow::bail!(
                "usage: lm-serve <file.pmtiles> [more.pmtiles ...]\n  \
                 [--addr host:port] [--base-url http://...] [--api-key <token>]\n  \
                 [--cors <origin>] [--timeout <secs>] [--max-in-flight <n>]\n  \
                 [--max-zoom <0-30>] [--metrics]"
            );
        }

        Ok((paths, cfg))
    }
}

fn next_val(args: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<String> {
    args.next().ok_or_else(|| anyhow::anyhow!("{flag} requires a value"))
}

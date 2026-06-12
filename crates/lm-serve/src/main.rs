use lm_serve::config::ServeConfig;
use lm_serve::server::run;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lm_serve=info".into()),
        )
        .init();

    let (paths, cfg) = ServeConfig::from_args(std::env::args().skip(1))?;
    run(paths, cfg).await
}

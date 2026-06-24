//! mortis-code-server binary entrypoint — a thin wrapper over
//! [`mortis_server::run`].

use anyhow::Context;
use camino::Utf8PathBuf;
use tracing_subscriber::{EnvFilter, fmt};

use mortis_server::{config::Config, run};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg_path = Utf8PathBuf::from(
        std::env::args().nth(1).unwrap_or_else(|| "config.toml".to_string()),
    );
    let config = Config::load(&cfg_path).with_context(|| format!("loading config {cfg_path}"))?;
    run(config).await
}

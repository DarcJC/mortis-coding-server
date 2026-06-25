//! mortis-code-server binary entrypoint — a thin wrapper over
//! [`mortis_server::run`].

use anyhow::Context;
use camino::Utf8PathBuf;

use mortis_server::{config::Config, run};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // When `git` invokes this executable as its GIT_ASKPASS program, answer the
    // credential prompt from the environment and exit before normal startup.
    // The git backend points GIT_ASKPASS at our own binary and passes the
    // username/password via MORTIS_GIT_* env vars (kept off the command line).
    if std::env::var_os("MORTIS_ASKPASS").is_some() {
        let prompt = std::env::args().nth(1).unwrap_or_default().to_lowercase();
        let key = if prompt.contains("username") {
            "MORTIS_GIT_USERNAME"
        } else {
            "MORTIS_GIT_PASSWORD"
        };
        println!("{}", std::env::var(key).unwrap_or_default());
        return Ok(());
    }

    // Logging is initialized inside `run` (it needs the loaded config), so a
    // config-load failure here is reported via anyhow to stderr.
    let cfg_path = Utf8PathBuf::from(
        std::env::args().nth(1).unwrap_or_else(|| "config.toml".to_string()),
    );
    let config = Config::load(&cfg_path).with_context(|| format!("loading config {cfg_path}"))?;
    run(config).await
}

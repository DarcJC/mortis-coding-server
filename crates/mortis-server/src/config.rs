//! Server configuration, loaded from `config.toml` (with `MORTIS_`-prefixed
//! environment overrides) via `figment`.

use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;

use mortis_core::RepoConfig;

/// Top-level configuration document.
#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    /// Repositories to serve (TOML `[[repo]]` tables).
    #[serde(default, rename = "repo")]
    pub repos: Vec<RepoConfig>,
    #[serde(default)]
    pub session: SessionConfig,
}

/// `[server]` section.
#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    /// Socket address to bind, e.g. `127.0.0.1:8080` or `0.0.0.0:8080`.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Root directory for materialized repos, sessions and caches.
    #[serde(default = "default_data_dir")]
    pub data_dir: Utf8PathBuf,
    /// Explicit path to an `svn` executable. Overrides embedded/system lookup.
    #[serde(default)]
    pub svn_bin: Option<Utf8PathBuf>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            data_dir: default_data_dir(),
            svn_bin: None,
        }
    }
}

/// `[auth]` section: a list of bearer tokens mapped to principals.
#[derive(Debug, Default, Deserialize)]
pub struct AuthConfig {
    #[serde(default)]
    pub tokens: Vec<TokenEntry>,
}

/// One `{ token, principal }` pair.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenEntry {
    pub token: String,
    pub principal: String,
}

/// `[session]` section.
#[derive(Debug, Deserialize)]
pub struct SessionConfig {
    /// Reserved: sessions are always persisted to disk in this implementation.
    #[serde(default = "default_true")]
    pub persist: bool,
    /// Idle time-to-live as a human duration (e.g. `"24h"`, `"30m"`).
    #[serde(default = "default_ttl")]
    pub ttl: String,
    /// How often the reaper runs, as a human duration.
    #[serde(default = "default_reap_interval")]
    pub reap_interval: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            persist: true,
            ttl: default_ttl(),
            reap_interval: default_reap_interval(),
        }
    }
}

impl SessionConfig {
    /// Parsed TTL, falling back to 24h on a malformed value.
    pub fn ttl_duration(&self) -> Duration {
        humantime::parse_duration(&self.ttl).unwrap_or(Duration::from_secs(24 * 3600))
    }

    /// Parsed reaper interval, falling back to 10 minutes.
    pub fn reap_duration(&self) -> Duration {
        humantime::parse_duration(&self.reap_interval).unwrap_or(Duration::from_secs(600))
    }
}

fn default_bind() -> String {
    "127.0.0.1:8080".to_string()
}
fn default_data_dir() -> Utf8PathBuf {
    Utf8PathBuf::from("./data")
}
fn default_true() -> bool {
    true
}
fn default_ttl() -> String {
    "24h".to_string()
}
fn default_reap_interval() -> String {
    "10m".to_string()
}

impl Config {
    /// Load configuration from a TOML file, applying `MORTIS_`-prefixed env
    /// overrides (e.g. `MORTIS_SERVER__BIND=0.0.0.0:9000`).
    pub fn load(path: &Utf8Path) -> anyhow::Result<Self> {
        use figment::{
            Figment,
            providers::{Env, Format, Toml},
        };
        let cfg = Figment::new()
            .merge(Toml::file(path.as_std_path()))
            .merge(Env::prefixed("MORTIS_").split("__"))
            .extract()?;
        Ok(cfg)
    }
}

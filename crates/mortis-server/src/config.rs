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
    #[serde(default)]
    pub asm: AsmConfig,
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

/// `[asm]` section: assembly-query sessions (binary download + disassembly).
#[derive(Debug, Deserialize)]
pub struct AsmConfig {
    /// Directory for downloaded binaries. Defaults to `<data_dir>/asm`.
    #[serde(default)]
    pub download_dir: Option<Utf8PathBuf>,
    /// Hosts permitted as download sources. **Deny-by-default**: an empty list
    /// rejects every download. An entry may be an exact host (`example.com`) or
    /// a leading-dot domain wildcard (`.example.com` matches subdomains too).
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Maximum download size in bytes (default 256 MiB).
    #[serde(default = "default_max_download_bytes")]
    pub max_download_bytes: u64,
    /// Per-request download timeout as a human duration (default `"60s"`).
    #[serde(default = "default_asm_timeout")]
    pub download_timeout: String,
    /// Idle time-to-live for an assembly session (default `"1h"`).
    #[serde(default = "default_asm_ttl")]
    pub session_ttl: String,
    /// Maximum number of concurrent assembly sessions (default 16).
    #[serde(default = "default_asm_max_sessions")]
    pub max_sessions: usize,
}

impl Default for AsmConfig {
    fn default() -> Self {
        Self {
            download_dir: None,
            allowed_hosts: Vec::new(),
            max_download_bytes: default_max_download_bytes(),
            download_timeout: default_asm_timeout(),
            session_ttl: default_asm_ttl(),
            max_sessions: default_asm_max_sessions(),
        }
    }
}

impl AsmConfig {
    /// Parsed download timeout, falling back to 60s on a malformed value.
    pub fn timeout_duration(&self) -> Duration {
        humantime::parse_duration(&self.download_timeout).unwrap_or(Duration::from_secs(60))
    }

    /// Parsed session TTL, falling back to 1h on a malformed value.
    pub fn ttl_duration(&self) -> Duration {
        humantime::parse_duration(&self.session_ttl).unwrap_or(Duration::from_secs(3600))
    }

    /// Build the download policy handed to the assembly store.
    pub fn policy(&self) -> mortis_core::asm::AsmDownloadPolicy {
        mortis_core::asm::AsmDownloadPolicy {
            allowed_hosts: self.allowed_hosts.clone(),
            max_download_bytes: self.max_download_bytes,
            timeout: self.timeout_duration(),
            max_sessions: self.max_sessions,
        }
    }
}

fn default_bind() -> String {
    "127.0.0.1:8080".to_string()
}
fn default_max_download_bytes() -> u64 {
    256 * 1024 * 1024
}
fn default_asm_timeout() -> String {
    "60s".to_string()
}
fn default_asm_ttl() -> String {
    "1h".to_string()
}
fn default_asm_max_sessions() -> usize {
    16
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

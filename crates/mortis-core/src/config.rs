//! Configuration types that the domain and infrastructure layers consume.
//!
//! These are deserialized from the operator's `config.toml` by the server, but
//! they live in `mortis-core` because the backends operate on them directly
//! (e.g. a `VcsBackend` needs the repo `url`, `rev`, and whitelist globs).

use serde::{Deserialize, Serialize};

use crate::model::RepoId;

/// Which version-control system backs a repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VcsKind {
    Git,
    Svn,
}

/// Declarative description of one repository to serve.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    /// Stable id, also used as the on-disk directory name.
    pub id: RepoId,
    /// Backend selector.
    pub kind: VcsKind,
    /// Remote URL (https/svn/svn+ssh, depending on `kind`).
    pub url: String,
    /// Branch/tag/commit (Git) or revision (SVN). `None` → backend default.
    #[serde(default)]
    pub rev: Option<String>,
    /// Update schedule: a 6-field cron expression, or a human duration like
    /// `"15m"`. `None` disables automatic updates.
    #[serde(default)]
    pub schedule: Option<String>,
    /// Whitelist globs. When non-empty, only matching paths are materialized.
    #[serde(default)]
    pub include: Vec<String>,
    /// Blacklist globs, applied after `include`.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Optional username for authenticated remotes.
    #[serde(default)]
    pub username: Option<String>,
    /// Optional password/token for authenticated remotes.
    #[serde(default)]
    pub password: Option<String>,
}

impl RepoConfig {
    /// Whether a whitelist is configured (otherwise everything is materialized).
    pub fn has_whitelist(&self) -> bool {
        !self.include.is_empty()
    }
}

//! The [`VcsBackend`] strategy trait and its associated read-side value types.
//!
//! Backends are *stateless strategies*: every method receives a [`RepoContext`]
//! describing which repo it is operating on and where that repo lives on disk.
//! This keeps a single `GixBackend` / `SvnCliBackend` instance reusable across
//! all configured repositories of its kind.

use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

use crate::config::{RepoConfig, VcsKind};
use crate::error::Result;
use crate::model::{FileContent, ReadRange, RepoId, Rev, Timestamp};

/// Everything a backend needs to act on one repository.
///
/// `root` is the per-repo data directory (e.g. `<data>/repos/<id>`); the
/// backend owns the layout beneath it. Helpers expose the two conventional
/// subdirectories.
#[derive(Debug, Clone, Copy)]
pub struct RepoContext<'a> {
    /// The declarative configuration for this repo.
    pub spec: &'a RepoConfig,
    /// The per-repo data directory.
    pub root: &'a Utf8Path,
}

impl<'a> RepoContext<'a> {
    pub fn new(spec: &'a RepoConfig, root: &'a Utf8Path) -> Self {
        Self { spec, root }
    }

    /// Read-only materialized working tree (the session base, the search root).
    pub fn work_dir(&self) -> Utf8PathBuf {
        self.root.join("work")
    }

    /// Backend-private storage (git object db, svn admin area, etc.).
    pub fn internal_dir(&self) -> Utf8PathBuf {
        self.root.join("vcs")
    }
}

/// The outcome of a successful [`VcsBackend::sync`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoSnapshot {
    pub repo: RepoId,
    /// Resolved head revision after the sync (commit sha / svn revnum).
    pub head: String,
    /// The materialized read-only working tree.
    pub base_path: Utf8PathBuf,
    /// When the sync completed.
    pub synced_at: Timestamp,
    /// Number of materialized files (post-whitelist).
    pub file_count: usize,
}

/// One line of blame output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlameLine {
    /// 1-based line number in the final file.
    pub line_no: u32,
    /// Commit that last touched this line.
    pub commit: String,
    pub author: String,
    pub author_email: String,
    pub time: Timestamp,
    /// First line of the commit message.
    pub summary: String,
    /// The line's text content.
    pub content: String,
}

/// One commit in a history listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Commit {
    pub id: String,
    pub author: String,
    pub author_email: String,
    pub time: Timestamp,
    pub summary: String,
    pub message: String,
    pub parents: Vec<String>,
}

/// Filters for a history query.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LogQuery {
    /// Cap on number of commits returned.
    #[serde(default)]
    pub max_count: Option<usize>,
    /// Number of leading commits to skip (for pagination).
    #[serde(default)]
    pub skip: Option<usize>,
}

/// A read-only version-control backend.
///
/// All operations are read-only by design: there is no commit, push, or
/// check-in. Writes happen exclusively in the session overlay layer.
#[async_trait]
pub trait VcsBackend: Send + Sync {
    /// Which VCS this backend speaks.
    fn kind(&self) -> VcsKind;

    /// Clone/checkout (first run) or fetch/update (subsequent runs), then
    /// materialize the whitelisted working tree. Returns a fresh snapshot.
    async fn sync(&self, ctx: &RepoContext<'_>) -> Result<RepoSnapshot>;

    /// List logical file paths present at `at`.
    async fn list_files(&self, ctx: &RepoContext<'_>, at: &Rev) -> Result<Vec<Utf8PathBuf>>;

    /// Read a file (optionally a slice) at a given revision.
    async fn read_file(
        &self,
        ctx: &RepoContext<'_>,
        path: &Utf8Path,
        at: &Rev,
        range: Option<ReadRange>,
    ) -> Result<FileContent>;

    /// Blame a file at a given revision.
    async fn blame(
        &self,
        ctx: &RepoContext<'_>,
        path: &Utf8Path,
        at: &Rev,
    ) -> Result<Vec<BlameLine>>;

    /// Commit history for the whole repo (`path == None`) or one file.
    async fn history(
        &self,
        ctx: &RepoContext<'_>,
        path: Option<&Utf8Path>,
        query: &LogQuery,
    ) -> Result<Vec<Commit>>;
}

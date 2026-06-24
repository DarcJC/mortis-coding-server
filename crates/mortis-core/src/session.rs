//! The [`SessionStore`] trait — the copy-on-write write layer.
//!
//! A session is an isolated, per-principal overlay on top of a repository's
//! read-only working tree. Writes and deletes land in the session's *upper*
//! layer; the base is never mutated. The store also produces git-style
//! `status`/`diff`, exports a unified patch, persists across restarts, and
//! reaps idle sessions.

use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::error::Result;
use crate::model::{Principal, RepoId, SessionId, Timestamp};
use crate::view::FileView;

/// Persistent metadata describing one CoW session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    /// The principal that created and exclusively owns this session.
    pub owner: Principal,
    /// The repository this session overlays.
    pub repo: RepoId,
    /// The base revision captured at creation time.
    pub base_rev: String,
    /// The read-only base working tree this overlay sits on.
    pub base_path: Utf8PathBuf,
    pub created: Timestamp,
    /// Last time the session was read or written; drives TTL reaping.
    pub last_accessed: Timestamp,
}

/// The nature of a change relative to the base tree (mirrors `git status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

/// One entry in a session's status report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileStatus {
    pub path: Utf8PathBuf,
    pub change: ChangeKind,
}

/// The copy-on-write session layer.
///
/// Methods that read or write a session also refresh its `last_accessed`
/// timestamp (implementations should call their own `touch` logic).
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Create a new session owned by `owner` over `repo` at `base_rev`.
    async fn create(
        &self,
        owner: &Principal,
        repo: &RepoId,
        base_rev: &str,
        base_path: &Utf8Path,
    ) -> Result<Session>;

    /// Fetch a session by id (error if missing).
    async fn get(&self, id: &SessionId) -> Result<Session>;

    /// List all sessions owned by `owner`.
    async fn list(&self, owner: &Principal) -> Result<Vec<Session>>;

    /// Delete a session and all of its overlay data.
    async fn delete(&self, id: &SessionId) -> Result<()>;

    /// Write (create or overwrite) a file in the session's upper layer.
    async fn write_file(&self, id: &SessionId, path: &Utf8Path, content: &[u8]) -> Result<()>;

    /// Delete a file from the session view (whiteout if it exists in the base).
    async fn delete_file(&self, id: &SessionId, path: &Utf8Path) -> Result<()>;

    /// Compute the change set relative to the base tree.
    async fn status(&self, id: &SessionId) -> Result<Vec<FileStatus>>;

    /// Unified diff for one file, or the whole session when `path == None`.
    async fn diff(&self, id: &SessionId, path: Option<&Utf8Path>) -> Result<String>;

    /// Export the entire session change set as a single git-apply-able patch.
    async fn export_patch(&self, id: &SessionId) -> Result<String>;

    /// Refresh `last_accessed` to now.
    async fn touch(&self, id: &SessionId) -> Result<()>;

    /// Delete every session idle for longer than `ttl`. Returns the count.
    async fn reap_expired(&self, ttl: Duration) -> Result<usize>;

    /// Build an overlay [`FileView`] for reads/search within the session.
    async fn view(&self, id: &SessionId) -> Result<Box<dyn FileView>>;
}

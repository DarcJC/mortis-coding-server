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
use std::collections::HashSet;
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

/// One exact-match search/replace edit applied to a file's current content.
///
/// `search` is matched literally (not a regex). With `all == false` the search
/// text must occur exactly once — zero matches or an ambiguous match is an
/// error, so an edit can never silently land in the wrong place. An empty
/// `search` is only valid as the sole edit creating a brand-new file (its
/// `replace` becomes the whole content).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Replacement {
    /// Literal text to find in the current file content.
    pub search: String,
    /// Text to substitute for `search`.
    pub replace: String,
    /// Replace every occurrence instead of requiring a unique match.
    #[serde(default)]
    pub all: bool,
}

/// How to edit a file in place: either a strict unified diff or an ordered set
/// of exact search/replace blocks. Both are applied to the file's *current*
/// view (upper layer if present, else the base), atomically inside the store.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileEdit {
    /// A standard/unified (git-style) diff. Applied strictly: context must match
    /// exactly or the whole edit fails with [`CoreError::Conflict`].
    UnifiedDiff(String),
    /// A sequence of literal search/replace edits, applied in order.
    SearchReplace(Vec<Replacement>),
}

/// The result of an [`SessionStore::edit_file`] application.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditOutcome {
    /// The (sanitized) path that was written.
    pub path: Utf8PathBuf,
    /// Whether the edit created the file or modified an existing one.
    pub change: ChangeKind,
    /// Size of the resulting file in bytes.
    pub bytes: usize,
    /// Number of hunks (unified diff) or replacements (search/replace) applied.
    pub applied: usize,
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

    /// Apply an in-place [`FileEdit`] to a file and write the result into the
    /// upper layer.
    ///
    /// The read-of-current-content, apply, and write MUST be atomic: the
    /// implementation has to hold the same lock as [`write_file`] across the
    /// whole read-modify-write so a concurrent writer cannot interleave. A diff
    /// that does not apply cleanly (or an unsatisfiable search/replace) fails
    /// with [`CoreError::Conflict`] and leaves the session untouched.
    async fn edit_file(
        &self,
        id: &SessionId,
        path: &Utf8Path,
        edit: FileEdit,
    ) -> Result<EditOutcome>;

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

    /// The set of base paths currently pinned by any live session (across all
    /// owners). Drives garbage collection of per-revision repo snapshots: a
    /// snapshot directory in this set must not be reclaimed.
    async fn referenced_bases(&self) -> Result<HashSet<Utf8PathBuf>>;

    /// Build an overlay [`FileView`] for reads/search within the session.
    async fn view(&self, id: &SessionId) -> Result<Box<dyn FileView>>;
}

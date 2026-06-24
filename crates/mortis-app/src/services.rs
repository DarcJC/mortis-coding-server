//! The application service facade.
//!
//! [`Services`] is the single entry point the presentation adapters (REST and
//! MCP) call into. It owns the repository registry plus the injected
//! [`SearchEngine`] and [`SessionStore`], and exposes one async method per
//! use-case. Both adapters call the *same* methods, which is what keeps the two
//! protocols equivalent with zero duplicated logic.
//!
//! Method groups (the logical "services" from the design):
//! - repositories: [`Services::list_repos`], [`Services::sync_repo`], [`Services::sync_all`]
//! - search: [`Services::search_repo`], [`Services::search_all`], [`Services::search_session`]
//! - files: [`Services::read_repo_file`], [`Services::read_session_file`]
//! - blame/history: [`Services::blame`], [`Services::history`]
//! - sessions: create/list/get/delete + write/delete/status/diff/patch

use std::sync::Arc;

use camino::Utf8Path;
use serde::Serialize;

use mortis_core::{
    BlameLine, Commit, CoreError, FileContent, LogQuery, Principal, ReadRange, RepoId, RepoSnapshot,
    Result, Rev, SearchEngine, SearchMatch, SearchQuery, Session, SessionId, SessionStore,
    Timestamp, VcsKind, slice_file_content,
};
use mortis_fs::PhysicalFileView;

use crate::registry::{RepoEntry, RepoRegistry};

/// A read-side summary of one repository, for `list_repos`.
#[derive(Debug, Clone, Serialize)]
pub struct RepoInfo {
    pub id: RepoId,
    pub kind: VcsKind,
    pub url: String,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    /// Head revision of the last successful sync, if any.
    pub head: Option<String>,
    pub synced_at: Option<Timestamp>,
    pub file_count: Option<usize>,
}

/// The application facade. Cheap to clone is *not* required — it is wrapped in an
/// `Arc` by the server and shared across requests.
pub struct Services {
    repos: Arc<RepoRegistry>,
    search: Arc<dyn SearchEngine>,
    sessions: Arc<dyn SessionStore>,
}

impl Services {
    pub fn new(
        repos: Arc<RepoRegistry>,
        search: Arc<dyn SearchEngine>,
        sessions: Arc<dyn SessionStore>,
    ) -> Self {
        Self { repos, search, sessions }
    }

    /// Access the underlying registry (used by the scheduler).
    pub fn registry(&self) -> &Arc<RepoRegistry> {
        &self.repos
    }

    // ----------------------------------------------------------------- repos

    /// Summaries of all configured repositories.
    pub fn list_repos(&self) -> Vec<RepoInfo> {
        self.repos
            .all()
            .into_iter()
            .map(|e| {
                let snap = e.snapshot();
                RepoInfo {
                    id: e.spec.id.clone(),
                    kind: e.spec.kind,
                    url: e.spec.url.clone(),
                    include: e.spec.include.clone(),
                    exclude: e.spec.exclude.clone(),
                    head: snap.as_ref().map(|s| s.head.clone()),
                    synced_at: snap.as_ref().map(|s| s.synced_at),
                    file_count: snap.as_ref().map(|s| s.file_count),
                }
            })
            .collect()
    }

    /// Sync one repository, recording the resulting snapshot.
    pub async fn sync_repo(&self, id: &RepoId) -> Result<RepoSnapshot> {
        let entry = self.repos.get(id)?;
        let snap = entry.backend.sync(&entry.context()).await?;
        entry.set_snapshot(snap.clone());
        Ok(snap)
    }

    /// Sync every configured repository, returning per-repo results.
    pub async fn sync_all(&self) -> Vec<(RepoId, Result<RepoSnapshot>)> {
        let mut out = Vec::new();
        for entry in self.repos.all() {
            let res = self.sync_one(&entry).await;
            out.push((entry.spec.id.clone(), res));
        }
        out
    }

    async fn sync_one(&self, entry: &Arc<RepoEntry>) -> Result<RepoSnapshot> {
        let snap = entry.backend.sync(&entry.context()).await?;
        entry.set_snapshot(snap.clone());
        Ok(snap)
    }

    // ---------------------------------------------------------------- search

    /// Search a single repository's materialized tree.
    pub async fn search_repo(&self, id: &RepoId, query: SearchQuery) -> Result<Vec<SearchMatch>> {
        let entry = self.repos.get(id)?;
        let view = PhysicalFileView::new(entry.work_dir());
        let mut hits = run_search(self.search.clone(), view, query).await?;
        for m in &mut hits {
            m.repo = Some(id.clone());
        }
        Ok(hits)
    }

    /// Search every repository, tagging matches with their repo id.
    pub async fn search_all(&self, query: SearchQuery) -> Result<Vec<SearchMatch>> {
        let max = query.max_results;
        let mut all = Vec::new();
        for entry in self.repos.all() {
            let view = PhysicalFileView::new(entry.work_dir());
            let mut hits = run_search(self.search.clone(), view, query.clone()).await?;
            let id = entry.spec.id.clone();
            for m in &mut hits {
                m.repo = Some(id.clone());
            }
            all.append(&mut hits);
            if let Some(limit) = max {
                if all.len() >= limit {
                    all.truncate(limit);
                    break;
                }
            }
        }
        Ok(all)
    }

    /// Search within a session's overlay view (owner-checked).
    pub async fn search_session(
        &self,
        principal: &Principal,
        session: &SessionId,
        query: SearchQuery,
    ) -> Result<Vec<SearchMatch>> {
        let s = self.authorize(principal, session).await?;
        let view = self.sessions.view(&s.id).await?;
        run_search_boxed(self.search.clone(), view, query).await
    }

    // ------------------------------------------------------------------ read

    /// Read a file from a repository at a revision (reads the object store, so
    /// any revision works — not just the materialized head).
    pub async fn read_repo_file(
        &self,
        id: &RepoId,
        path: &Utf8Path,
        rev: &Rev,
        range: Option<ReadRange>,
    ) -> Result<FileContent> {
        let entry = self.repos.get(id)?;
        entry.backend.read_file(&entry.context(), path, rev, range).await
    }

    /// Read a file through a session's overlay view (owner-checked).
    pub async fn read_session_file(
        &self,
        principal: &Principal,
        session: &SessionId,
        path: &Utf8Path,
        range: Option<ReadRange>,
    ) -> Result<FileContent> {
        let s = self.authorize(principal, session).await?;
        let view = self.sessions.view(&s.id).await?;
        let path = path.to_owned();
        let bytes = tokio::task::spawn_blocking(move || view.read(&path).map(|b| (path, b)))
            .await
            .map_err(blocking_err)??;
        Ok(slice_file_content(bytes.0, &bytes.1, range))
    }

    // ----------------------------------------------------------- blame / log

    /// Blame a file at a revision against the original repository.
    pub async fn blame(&self, id: &RepoId, path: &Utf8Path, rev: &Rev) -> Result<Vec<BlameLine>> {
        let entry = self.repos.get(id)?;
        entry.backend.blame(&entry.context(), path, rev).await
    }

    /// Commit history for the repo or one file.
    pub async fn history(
        &self,
        id: &RepoId,
        path: Option<&Utf8Path>,
        query: &LogQuery,
    ) -> Result<Vec<Commit>> {
        let entry = self.repos.get(id)?;
        entry.backend.history(&entry.context(), path, query).await
    }

    // --------------------------------------------------------------- session

    /// Create a session over a repo's current head (must be synced first).
    pub async fn create_session(&self, principal: &Principal, repo: &RepoId) -> Result<Session> {
        let entry = self.repos.get(repo)?;
        let snap = entry.snapshot().ok_or_else(|| {
            CoreError::Conflict(format!("repo {repo} has not been synced yet"))
        })?;
        self.sessions
            .create(principal, repo, &snap.head, &snap.base_path)
            .await
    }

    /// List the caller's sessions.
    pub async fn list_sessions(&self, principal: &Principal) -> Result<Vec<Session>> {
        self.sessions.list(principal).await
    }

    /// Fetch one of the caller's sessions.
    pub async fn get_session(&self, principal: &Principal, id: &SessionId) -> Result<Session> {
        self.authorize(principal, id).await
    }

    /// Delete one of the caller's sessions.
    pub async fn delete_session(&self, principal: &Principal, id: &SessionId) -> Result<()> {
        self.authorize(principal, id).await?;
        self.sessions.delete(id).await
    }

    /// Write a file into the session's CoW layer.
    pub async fn write_file(
        &self,
        principal: &Principal,
        id: &SessionId,
        path: &Utf8Path,
        content: &[u8],
    ) -> Result<()> {
        self.authorize(principal, id).await?;
        self.sessions.write_file(id, path, content).await
    }

    /// Delete a file in the session view (whiteout if present in the base).
    pub async fn delete_file(
        &self,
        principal: &Principal,
        id: &SessionId,
        path: &Utf8Path,
    ) -> Result<()> {
        self.authorize(principal, id).await?;
        self.sessions.delete_file(id, path).await
    }

    /// Git-style status of the session.
    pub async fn session_status(
        &self,
        principal: &Principal,
        id: &SessionId,
    ) -> Result<Vec<mortis_core::FileStatus>> {
        self.authorize(principal, id).await?;
        self.sessions.status(id).await
    }

    /// Unified diff for one file or the whole session.
    pub async fn session_diff(
        &self,
        principal: &Principal,
        id: &SessionId,
        path: Option<&Utf8Path>,
    ) -> Result<String> {
        self.authorize(principal, id).await?;
        self.sessions.diff(id, path).await
    }

    /// Export the session's full change set as a git-apply-able patch.
    pub async fn export_patch(&self, principal: &Principal, id: &SessionId) -> Result<String> {
        self.authorize(principal, id).await?;
        self.sessions.export_patch(id).await
    }

    /// Reap idle sessions (called by the background reaper).
    pub async fn reap_sessions(&self, ttl: std::time::Duration) -> Result<usize> {
        self.sessions.reap_expired(ttl).await
    }

    // ----------------------------------------------------------------- guts

    /// Load a session and verify the caller owns it.
    async fn authorize(&self, principal: &Principal, id: &SessionId) -> Result<Session> {
        let s = self.sessions.get(id).await?;
        if &s.owner != principal {
            return Err(CoreError::Forbidden(format!(
                "session {id} is not owned by {principal}"
            )));
        }
        self.sessions.touch(id).await.ok();
        Ok(s)
    }
}

/// Run a search over a concrete view on the blocking pool.
async fn run_search(
    engine: Arc<dyn SearchEngine>,
    view: PhysicalFileView,
    query: SearchQuery,
) -> Result<Vec<SearchMatch>> {
    tokio::task::spawn_blocking(move || engine.search(&view, &query))
        .await
        .map_err(blocking_err)?
}

/// Run a search over a boxed (overlay) view on the blocking pool.
async fn run_search_boxed(
    engine: Arc<dyn SearchEngine>,
    view: Box<dyn mortis_core::FileView>,
    query: SearchQuery,
) -> Result<Vec<SearchMatch>> {
    tokio::task::spawn_blocking(move || engine.search(&*view, &query))
        .await
        .map_err(blocking_err)?
}

fn blocking_err(e: tokio::task::JoinError) -> CoreError {
    CoreError::Other(format!("blocking task failed: {e}"))
}

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

use std::collections::HashSet;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use serde::Serialize;

use mortis_core::{
    AsmSession, AsmSessionId, AssemblyStore, BinaryInfo, BlameLine, Commit, CoreError, Disassembly,
    FileContent, FunctionResolution, LogQuery, Principal, ReadRange, RepoId, RepoSnapshot, Result,
    Rev, SearchEngine, SearchMatch, SearchQuery, Session, SessionId, SessionStore, Timestamp,
    VcsKind, ensure_safe_relative, slice_file_content,
};
use mortis_fs::PhysicalFileView;

use crate::registry::{RepoEntry, RepoRegistry};

/// Max repositories processed in parallel for per-repo disk fan-out
/// ([`Services::search_all`] and [`Services::rehydrate_all`]).
///
/// Each per-repo task offloads a blocking walk (ripgrep / `count_files`) onto
/// the blocking pool, so this bounds both simultaneous blocking tasks and
/// independent disk walks. Kept small: on a cold page cache / single spinning
/// disk (the cold-start failure mode), unbounded fan-out just maximizes seek
/// thrash. Four overlaps enough to hide per-repo latency on typical multi-repo
/// configs without saturating one disk, and is far under tokio's default
/// 512-thread blocking pool. Tunable up for SSD/NVMe.
const REPO_FANOUT_CONCURRENCY: usize = 4;

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
    asm: Arc<dyn AssemblyStore>,
}

impl Services {
    pub fn new(
        repos: Arc<RepoRegistry>,
        search: Arc<dyn SearchEngine>,
        sessions: Arc<dyn SessionStore>,
        asm: Arc<dyn AssemblyStore>,
    ) -> Self {
        Self { repos, search, sessions, asm }
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
        self.sync_one(&entry).await
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

    /// Populate each repo's in-memory snapshot from the newest on-disk snapshot
    /// (no network), so reads resolve a valid base immediately at startup —
    /// before the background initial sync has run. Repos are processed with
    /// bounded concurrency so a many-repo startup isn't delayed by the serial
    /// sum of per-repo directory walks. Best-effort: per-repo failures are
    /// logged and skipped, never fatal.
    pub async fn rehydrate_all(&self) {
        use futures::stream::{self, StreamExt};

        stream::iter(self.repos.all())
            .for_each_concurrent(REPO_FANOUT_CONCURRENCY, |entry| async move {
                match entry.backend.rehydrate(&entry.context()).await {
                    Ok(Some(snap)) => {
                        tracing::info!(
                            repo = %entry.spec.id, head = %snap.head, files = snap.file_count,
                            "rehydrated snapshot from disk"
                        );
                        entry.set_snapshot(snap);
                    }
                    Ok(None) => {
                        tracing::debug!(repo = %entry.spec.id, "no on-disk snapshot to rehydrate");
                    }
                    Err(e) => {
                        tracing::warn!(repo = %entry.spec.id, error = %e, "snapshot rehydrate failed");
                    }
                }
            })
            .await;
    }

    async fn sync_one(&self, entry: &Arc<RepoEntry>) -> Result<RepoSnapshot> {
        // Serialize syncs of this repo so the snapshot publish + GC below can't
        // race a concurrent (scheduled or manual) sync of the same repo.
        let _guard = entry.sync_lock().await;
        let snap = entry.backend.sync(&entry.context()).await?;
        entry.set_snapshot(snap.clone());
        // Reclaim snapshot dirs that are now neither current nor session-pinned.
        self.gc_snapshots(entry).await;
        Ok(snap)
    }

    /// GC the snapshot directories of a single repo after a sync.
    async fn gc_snapshots(&self, entry: &RepoEntry) {
        match self.sessions.referenced_bases().await {
            Ok(referenced) => gc_entry(entry, &referenced),
            Err(e) => tracing::warn!(
                repo = %entry.spec.id,
                error = %e,
                "snapshot GC skipped: cannot list session-referenced bases"
            ),
        }
    }

    /// GC every repo's snapshot directories, reusing a single referenced-bases
    /// scan. Called by the reaper after expiring idle sessions.
    pub async fn gc_all_snapshots(&self) {
        let referenced = match self.sessions.referenced_bases().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "snapshot GC skipped: cannot list session-referenced bases");
                return;
            }
        };
        for entry in self.repos.all() {
            gc_entry(&entry, &referenced);
        }
    }

    // ---------------------------------------------------------------- search

    /// Search a single repository's materialized tree.
    pub async fn search_repo(&self, id: &RepoId, query: SearchQuery) -> Result<Vec<SearchMatch>> {
        let entry = self.repos.get(id)?;
        let Some(base) = entry.current_base() else {
            return Ok(Vec::new()); // not synced yet → no results (never walk the parent dir)
        };
        let view = PhysicalFileView::new(base);
        let mut hits = run_search(self.search.clone(), view, query).await?;
        for m in &mut hits {
            m.repo = Some(id.clone());
        }
        Ok(hits)
    }

    /// Search every repository concurrently (bounded), tagging matches with
    /// their repo id. Repos are searched in repo-id order so a `max_results`
    /// truncation keeps a reproducible subset; the search stops pulling once the
    /// cap is met rather than walking every repo.
    pub async fn search_all(&self, query: SearchQuery) -> Result<Vec<SearchMatch>> {
        use futures::stream::{self, StreamExt};

        let max = query.max_results;
        // Stable id order: `repos.all()` is HashMap-arbitrary, so without this
        // the surviving subset under `max_results` would vary across runs.
        let mut entries = self.repos.all();
        entries.sort_by(|a, b| a.spec.id.0.cmp(&b.spec.id.0));

        // `buffered` runs up to N searches concurrently while yielding results
        // in id order; we stop draining once `max_results` is satisfied so a
        // capped query needn't walk every repo. (Any still-in-flight searches
        // past the cap are dropped; their detached blocking walks finish unused.)
        let mut stream = stream::iter(entries)
            .map(|entry| {
                let search = self.search.clone();
                let query = query.clone();
                async move {
                    let Some(base) = entry.current_base() else {
                        return Ok::<Vec<SearchMatch>, CoreError>(Vec::new());
                    };
                    let view = PhysicalFileView::new(base);
                    let mut hits = run_search(search, view, query).await?;
                    let id = entry.spec.id.clone();
                    for m in &mut hits {
                        m.repo = Some(id.clone());
                    }
                    Ok(hits)
                }
            })
            .buffered(REPO_FANOUT_CONCURRENCY);

        let mut all = Vec::new();
        while let Some(hits) = stream.next().await {
            all.extend(hits?);
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
        // Confine the request path: for SVN the path is interpolated into the
        // peg-revision target URL, where a `..` would escape the configured
        // repo subtree on the server.
        let path = ensure_safe_relative(path)?;
        let entry = self.repos.get(id)?;
        entry.backend.read_file(&entry.context(), &path, rev, range).await
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
        let path = ensure_safe_relative(path)?;
        let entry = self.repos.get(id)?;
        entry.backend.blame(&entry.context(), &path, rev).await
    }

    /// Commit history for the repo or one file.
    pub async fn history(
        &self,
        id: &RepoId,
        path: Option<&Utf8Path>,
        query: &LogQuery,
    ) -> Result<Vec<Commit>> {
        let path = path.map(ensure_safe_relative).transpose()?;
        let entry = self.repos.get(id)?;
        entry.backend.history(&entry.context(), path.as_deref(), query).await
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

    /// Apply an in-place edit (unified diff or search/replace blocks) to a
    /// session file. Owner-checked; the store performs the read-modify-write
    /// atomically, so this facade does not pre-read the file.
    pub async fn edit_file(
        &self,
        principal: &Principal,
        id: &SessionId,
        path: &Utf8Path,
        edit: mortis_core::FileEdit,
    ) -> Result<mortis_core::EditOutcome> {
        self.authorize(principal, id).await?;
        self.sessions.edit_file(id, path, edit).await
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

    // ------------------------------------------------------- assembly sessions

    /// Create an assembly session: download `url` and validate it in the
    /// background. Returns immediately with the session in a downloading state.
    pub async fn create_asm_session(
        &self,
        principal: &Principal,
        url: &str,
    ) -> Result<AsmSession> {
        self.asm.create(principal, url).await
    }

    /// List the caller's assembly sessions.
    pub async fn list_asm_sessions(&self, principal: &Principal) -> Result<Vec<AsmSession>> {
        self.asm.list(principal).await
    }

    /// Fetch one of the caller's assembly sessions (status, progress, result).
    pub async fn get_asm_session(
        &self,
        principal: &Principal,
        id: &AsmSessionId,
    ) -> Result<AsmSession> {
        self.authorize_asm(principal, id).await
    }

    /// Delete one of the caller's assembly sessions.
    pub async fn delete_asm_session(
        &self,
        principal: &Principal,
        id: &AsmSessionId,
    ) -> Result<()> {
        self.authorize_asm(principal, id).await?;
        self.asm.delete(id).await
    }

    /// Disassemble an address range in the caller's assembly session.
    pub async fn asm_disassemble(
        &self,
        principal: &Principal,
        id: &AsmSessionId,
        start: u64,
        len: u64,
    ) -> Result<Disassembly> {
        self.authorize_asm(principal, id).await?;
        self.asm.disassemble(id, start, len).await
    }

    /// Resolve an address to a function name in the caller's assembly session.
    pub async fn asm_resolve_function(
        &self,
        principal: &Principal,
        id: &AsmSessionId,
        address: u64,
    ) -> Result<FunctionResolution> {
        self.authorize_asm(principal, id).await?;
        self.asm.resolve_function(id, address).await
    }

    /// Header/section metadata of the caller's assembly-session binary.
    pub async fn asm_metadata(
        &self,
        principal: &Principal,
        id: &AsmSessionId,
    ) -> Result<BinaryInfo> {
        self.authorize_asm(principal, id).await?;
        self.asm.metadata(id).await
    }

    /// Reap idle assembly sessions (called by the background reaper).
    pub async fn reap_asm_sessions(&self, ttl: std::time::Duration) -> Result<usize> {
        self.asm.reap_expired(ttl).await
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

    /// Load an assembly session and verify the caller owns it.
    async fn authorize_asm(&self, principal: &Principal, id: &AsmSessionId) -> Result<AsmSession> {
        let s = self.asm.get(id).await?;
        if &s.owner != principal {
            return Err(CoreError::Forbidden(format!(
                "assembly session {id} is not owned by {principal}"
            )));
        }
        self.asm.touch(id).await.ok();
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

/// Remove `entry`'s snapshot directories that are neither the current snapshot
/// nor pinned by a live session, plus a legacy pre-upgrade `work/` dir once it
/// is unreferenced. Best-effort: failures are logged and retried next GC pass.
///
/// `.staging-*` directories are always skipped — a concurrent publish owns them.
fn gc_entry(entry: &RepoEntry, referenced: &HashSet<Utf8PathBuf>) {
    let ctx = entry.context();
    let current = entry.snapshot().map(|s| s.base_path);
    let keep = |p: &Utf8PathBuf| current.as_ref() == Some(p) || referenced.contains(p);

    if let Ok(read) = std::fs::read_dir(ctx.snapshots_dir()) {
        for child in read.flatten() {
            let Ok(path) = Utf8PathBuf::from_path_buf(child.path()) else {
                continue;
            };
            let is_staging = path
                .file_name()
                .is_some_and(|n| n.starts_with(".staging-"));
            if is_staging || keep(&path) {
                continue;
            }
            if let Err(e) = std::fs::remove_dir_all(&path) {
                tracing::warn!(repo = %entry.spec.id, dir = %path, error = %e,
                    "failed to reclaim snapshot dir");
            }
        }
    }

    // Legacy pre-upgrade `work/`: reclaim once no session pins it (a session
    // created before the upgrade keeps it alive until reaped/deleted).
    let legacy = ctx.work_dir();
    if legacy.exists() && !keep(&legacy) {
        let _ = std::fs::remove_dir_all(&legacy);
    }
}

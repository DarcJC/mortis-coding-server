//! The repository registry: the single source of truth mapping a [`RepoId`] to
//! its configuration, on-disk location, the [`VcsBackend`] strategy that serves
//! it, and the most recent sync snapshot.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use camino::{Utf8Path, Utf8PathBuf};

use mortis_core::vcs::RepoContext;
use mortis_core::{CoreError, RepoConfig, RepoId, RepoSnapshot, Result, VcsBackend, VcsKind};

/// One configured repository plus its mutable sync state.
pub struct RepoEntry {
    /// Declarative configuration.
    pub spec: RepoConfig,
    /// Per-repo data directory (`<data>/repos/<id>`).
    pub root: Utf8PathBuf,
    /// The backend strategy matching `spec.kind`.
    pub backend: Arc<dyn VcsBackend>,
    /// The latest successful sync, if any. Drives session base resolution.
    snapshot: RwLock<Option<RepoSnapshot>>,
    /// Serializes syncs of THIS repo so a scheduled and a manual sync can't race
    /// the snapshot publish or the post-sync GC. Distinct repos sync in parallel.
    sync_mutex: tokio::sync::Mutex<()>,
    /// Active reader/creator leases on snapshot bases (base_path → count). A
    /// non-zero count pins that base against GC while a search walks it or a
    /// session-create persists. A std `Mutex` — never held across `.await`.
    leases: Mutex<HashMap<Utf8PathBuf, usize>>,
}

impl RepoEntry {
    /// Build the borrow-based context handed to backend calls.
    pub fn context(&self) -> RepoContext<'_> {
        RepoContext::new(&self.spec, &self.root)
    }

    /// Acquire this repo's sync lock (held across the backend sync + snapshot
    /// update + GC).
    pub async fn sync_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.sync_mutex.lock().await
    }

    /// The materialized read-only working tree (valid after a sync).
    pub fn work_dir(&self) -> Utf8PathBuf {
        self.context().work_dir()
    }

    /// A clone of the latest snapshot, if the repo has been synced.
    pub fn snapshot(&self) -> Option<RepoSnapshot> {
        self.snapshot.read().expect("snapshot lock poisoned").clone()
    }

    /// The current read/search base: the latest snapshot's materialized tree,
    /// or `None` if the repo has no snapshot yet.
    ///
    /// `None` means "contributes no search results". Callers must NOT fall back
    /// to the snapshots parent dir — that directory holds *every* published head
    /// (and any in-flight `.staging-*`), so walking it would search stale and
    /// partial trees and blow the search budget on a cold cache.
    pub fn current_base(&self) -> Option<Utf8PathBuf> {
        self.snapshot().map(|s| s.base_path)
    }

    /// Record a fresh snapshot after a successful sync.
    pub fn set_snapshot(&self, snap: RepoSnapshot) {
        *self.snapshot.write().expect("snapshot lock poisoned") = Some(snap);
    }

    /// Lease the current snapshot so GC will not reclaim its base while the
    /// returned [`BaseLease`] is alive. Returns the snapshot + guard, or `None`
    /// if the repo has no snapshot yet.
    ///
    /// Reads the current base and increments its lease under a single `leases`
    /// lock hold, so the (read-current, increment) pair is atomic against
    /// [`gc_protected`](Self::gc_protected). A reader therefore always leases
    /// whatever GC observes as current — which GC never deletes.
    pub fn lease_current(self: &Arc<Self>) -> Option<BaseLease> {
        let mut leases = self.leases.lock().expect("leases lock poisoned");
        let snapshot = self.snapshot()?;
        *leases.entry(snapshot.base_path.clone()).or_insert(0) += 1;
        Some(BaseLease { entry: Arc::clone(self), snapshot })
    }

    /// Snapshot the GC keep-inputs atomically: the current base plus every
    /// leased base, read under one `leases` lock hold.
    ///
    /// Reading `current` here (rather than via a separate later `snapshot()`)
    /// is load-bearing: it makes the keep-set consistent with concurrent
    /// [`lease_current`](Self::lease_current) calls, closing the window where a
    /// reader could lease a base GC is about to delete.
    pub(crate) fn gc_protected(&self) -> (Option<Utf8PathBuf>, HashSet<Utf8PathBuf>) {
        let leases = self.leases.lock().expect("leases lock poisoned");
        let current = self.snapshot().map(|s| s.base_path);
        let leased = leases.keys().cloned().collect();
        (current, leased)
    }

    /// Drop one lease on `base` (called by [`BaseLease`]'s `Drop`).
    fn release_lease(&self, base: &Utf8Path) {
        let mut leases = self.leases.lock().expect("leases lock poisoned");
        if let Some(count) = leases.get_mut(base) {
            *count -= 1;
            if *count == 0 {
                leases.remove(base);
            }
        }
    }
}

/// An RAII lease pinning a repository's snapshot base against GC for as long as
/// it is held. Obtain via [`RepoEntry::lease_current`]; dropping it releases the
/// pin. Holds an `Arc<RepoEntry>` so the entry outlives the lease.
pub struct BaseLease {
    entry: Arc<RepoEntry>,
    snapshot: RepoSnapshot,
}

impl BaseLease {
    /// The leased snapshot.
    pub fn snapshot(&self) -> &RepoSnapshot {
        &self.snapshot
    }

    /// The leased base path (the materialized read-only tree).
    pub fn base_path(&self) -> &Utf8Path {
        &self.snapshot.base_path
    }
}

impl Drop for BaseLease {
    fn drop(&mut self) {
        self.entry.release_lease(&self.snapshot.base_path);
    }
}

/// An immutable collection of [`RepoEntry`] keyed by id.
pub struct RepoRegistry {
    entries: HashMap<RepoId, Arc<RepoEntry>>,
}

/// Factory selecting a backend for a given [`VcsKind`].
///
/// The server constructs the concrete backends (e.g. `GixBackend`) and supplies
/// them here, keeping `mortis-app` free of any concrete VCS dependency.
#[derive(Clone)]
pub struct BackendSet {
    pub git: Arc<dyn VcsBackend>,
    pub svn: Option<Arc<dyn VcsBackend>>,
}

impl BackendSet {
    fn select(&self, kind: VcsKind) -> Result<Arc<dyn VcsBackend>> {
        match kind {
            VcsKind::Git => Ok(self.git.clone()),
            VcsKind::Svn => self
                .svn
                .clone()
                .ok_or_else(|| CoreError::Config("SVN backend is not enabled".into())),
        }
    }
}

impl RepoRegistry {
    /// Build the registry from configured repos, a data directory, and the
    /// available backends.
    pub fn build(
        repos: Vec<RepoConfig>,
        data_dir: &Utf8Path,
        backends: &BackendSet,
    ) -> Result<Self> {
        let repos_root = data_dir.join("repos");
        let mut entries = HashMap::new();
        for spec in repos {
            if entries.contains_key(&spec.id) {
                return Err(CoreError::Config(format!("duplicate repo id: {}", spec.id)));
            }
            let backend = backends.select(spec.kind)?;
            let root = repos_root.join(&spec.id.0);
            let id = spec.id.clone();
            entries.insert(
                id,
                Arc::new(RepoEntry {
                    spec,
                    root,
                    backend,
                    snapshot: RwLock::new(None),
                    sync_mutex: tokio::sync::Mutex::new(()),
                    leases: Mutex::new(HashMap::new()),
                }),
            );
        }
        Ok(Self { entries })
    }

    /// Look up a repo by id.
    pub fn get(&self, id: &RepoId) -> Result<Arc<RepoEntry>> {
        self.entries
            .get(id)
            .cloned()
            .ok_or_else(|| CoreError::not_found(format!("repo {id}")))
    }

    /// All entries, in arbitrary order.
    pub fn all(&self) -> Vec<Arc<RepoEntry>> {
        self.entries.values().cloned().collect()
    }

    /// Number of configured repos.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no repos are configured.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

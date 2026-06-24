//! The repository registry: the single source of truth mapping a [`RepoId`] to
//! its configuration, on-disk location, the [`VcsBackend`] strategy that serves
//! it, and the most recent sync snapshot.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

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
}

impl RepoEntry {
    /// Build the borrow-based context handed to backend calls.
    pub fn context(&self) -> RepoContext<'_> {
        RepoContext::new(&self.spec, &self.root)
    }

    /// The materialized read-only working tree (valid after a sync).
    pub fn work_dir(&self) -> Utf8PathBuf {
        self.context().work_dir()
    }

    /// A clone of the latest snapshot, if the repo has been synced.
    pub fn snapshot(&self) -> Option<RepoSnapshot> {
        self.snapshot.read().expect("snapshot lock poisoned").clone()
    }

    /// Record a fresh snapshot after a successful sync.
    pub fn set_snapshot(&self, snap: RepoSnapshot) {
        *self.snapshot.write().expect("snapshot lock poisoned") = Some(snap);
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

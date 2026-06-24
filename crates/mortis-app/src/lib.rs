//! # mortis-app
//!
//! The application service layer: a thin orchestration [`Services`] facade over
//! the domain ports defined in `mortis-core`. It depends only on those traits
//! plus `mortis-fs` for building file views — never on a concrete VCS, search,
//! or session implementation. The server injects those at startup.

pub mod registry;
pub mod services;

pub use registry::{BackendSet, RepoEntry, RepoRegistry};
pub use services::{RepoInfo, Services};

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use camino::{Utf8Path, Utf8PathBuf};

    use mortis_core::vcs::RepoContext;
    use mortis_core::{
        AsmSession, AsmSessionId, AssemblyStore, BinaryInfo, BlameLine, Commit, CoreError,
        Disassembly, FileContent, FileView, FunctionResolution, LogQuery, Principal, ReadRange,
        RepoConfig, RepoId, RepoSnapshot, Result, Rev, SearchEngine, SearchMatch, SearchQuery,
        Session, SessionId, SessionStore, Timestamp, VcsBackend, VcsKind, slice_file_content,
    };

    use super::*;

    // ---- minimal fakes (only the methods our tests reach are meaningful) ----

    struct FakeGit;
    #[async_trait]
    impl VcsBackend for FakeGit {
        fn kind(&self) -> VcsKind {
            VcsKind::Git
        }
        async fn sync(&self, ctx: &RepoContext<'_>) -> Result<RepoSnapshot> {
            let base = ctx.snapshot_dir("deadbeef");
            std::fs::create_dir_all(&base).ok();
            std::fs::write(base.join("a.txt"), b"hello\nworld\n").ok();
            Ok(RepoSnapshot {
                repo: ctx.spec.id.clone(),
                head: "deadbeef".into(),
                base_path: base,
                synced_at: Timestamp(1),
                file_count: 1,
            })
        }
        async fn list_files(&self, _c: &RepoContext<'_>, _a: &Rev) -> Result<Vec<Utf8PathBuf>> {
            Ok(vec!["a.txt".into()])
        }
        async fn read_file(
            &self,
            _c: &RepoContext<'_>,
            path: &Utf8Path,
            _a: &Rev,
            range: Option<ReadRange>,
        ) -> Result<FileContent> {
            Ok(slice_file_content(path.to_owned(), b"hello\nworld\n", range))
        }
        async fn blame(&self, _c: &RepoContext<'_>, _p: &Utf8Path, _a: &Rev) -> Result<Vec<BlameLine>> {
            Ok(vec![])
        }
        async fn history(
            &self,
            _c: &RepoContext<'_>,
            _p: Option<&Utf8Path>,
            _q: &LogQuery,
        ) -> Result<Vec<Commit>> {
            Ok(vec![])
        }
    }

    struct NoSearch;
    impl SearchEngine for NoSearch {
        fn search_streaming(
            &self,
            _v: &dyn FileView,
            _q: &SearchQuery,
            _s: &mut dyn FnMut(SearchMatch) -> mortis_core::Flow,
        ) -> Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct NoSessions {
        /// Base paths a test wants `referenced_bases` to report (drives GC tests).
        bases: std::sync::Mutex<std::collections::HashSet<Utf8PathBuf>>,
    }
    #[async_trait]
    impl SessionStore for NoSessions {
        async fn create(
            &self,
            owner: &Principal,
            repo: &RepoId,
            base_rev: &str,
            base_path: &Utf8Path,
        ) -> Result<Session> {
            Ok(Session {
                id: SessionId::from("s1"),
                owner: owner.clone(),
                repo: repo.clone(),
                base_rev: base_rev.to_owned(),
                base_path: base_path.to_owned(),
                created: Timestamp(1),
                last_accessed: Timestamp(1),
            })
        }
        async fn get(&self, id: &SessionId) -> Result<Session> {
            Err(CoreError::not_found(id.0.clone()))
        }
        async fn list(&self, _o: &Principal) -> Result<Vec<Session>> {
            Ok(vec![])
        }
        async fn delete(&self, _id: &SessionId) -> Result<()> {
            Ok(())
        }
        async fn write_file(&self, _id: &SessionId, _p: &Utf8Path, _c: &[u8]) -> Result<()> {
            Ok(())
        }
        async fn edit_file(
            &self,
            id: &SessionId,
            _p: &Utf8Path,
            _e: mortis_core::FileEdit,
        ) -> Result<mortis_core::EditOutcome> {
            Err(CoreError::not_found(id.0.clone()))
        }
        async fn delete_file(&self, _id: &SessionId, _p: &Utf8Path) -> Result<()> {
            Ok(())
        }
        async fn status(&self, _id: &SessionId) -> Result<Vec<mortis_core::FileStatus>> {
            Ok(vec![])
        }
        async fn diff(&self, _id: &SessionId, _p: Option<&Utf8Path>) -> Result<String> {
            Ok(String::new())
        }
        async fn export_patch(&self, _id: &SessionId) -> Result<String> {
            Ok(String::new())
        }
        async fn touch(&self, _id: &SessionId) -> Result<()> {
            Ok(())
        }
        async fn reap_expired(&self, _ttl: std::time::Duration) -> Result<usize> {
            Ok(0)
        }
        async fn referenced_bases(&self) -> Result<std::collections::HashSet<Utf8PathBuf>> {
            Ok(self.bases.lock().unwrap().clone())
        }
        async fn view(&self, id: &SessionId) -> Result<Box<dyn FileView>> {
            Err(CoreError::not_found(id.0.clone()))
        }
    }

    struct NoAsm;
    #[async_trait]
    impl AssemblyStore for NoAsm {
        async fn create(&self, _o: &Principal, _u: &str) -> Result<AsmSession> {
            Err(CoreError::Other("no asm store".into()))
        }
        async fn get(&self, id: &AsmSessionId) -> Result<AsmSession> {
            Err(CoreError::not_found(id.0.clone()))
        }
        async fn list(&self, _o: &Principal) -> Result<Vec<AsmSession>> {
            Ok(vec![])
        }
        async fn delete(&self, id: &AsmSessionId) -> Result<()> {
            Err(CoreError::not_found(id.0.clone()))
        }
        async fn disassemble(&self, id: &AsmSessionId, _s: u64, _l: u64) -> Result<Disassembly> {
            Err(CoreError::not_found(id.0.clone()))
        }
        async fn resolve_function(
            &self,
            id: &AsmSessionId,
            _a: u64,
        ) -> Result<FunctionResolution> {
            Err(CoreError::not_found(id.0.clone()))
        }
        async fn metadata(&self, id: &AsmSessionId) -> Result<BinaryInfo> {
            Err(CoreError::not_found(id.0.clone()))
        }
        async fn touch(&self, _id: &AsmSessionId) -> Result<()> {
            Ok(())
        }
        async fn reap_expired(&self, _ttl: std::time::Duration) -> Result<usize> {
            Ok(0)
        }
    }

    fn spec(id: &str, kind: VcsKind) -> RepoConfig {
        RepoConfig {
            id: RepoId::from(id),
            kind,
            url: "file:///x".into(),
            rev: None,
            schedule: None,
            include: vec![],
            exclude: vec![],
            username: None,
            password: None,
        }
    }

    fn services_over(tmp: &Utf8Path, repos: Vec<RepoConfig>) -> Services {
        let backends = BackendSet { git: Arc::new(FakeGit), svn: None };
        let reg = Arc::new(RepoRegistry::build(repos, tmp, &backends).unwrap());
        Services::new(
            reg,
            Arc::new(NoSearch),
            Arc::new(NoSessions::default()),
            Arc::new(NoAsm),
        )
    }

    #[tokio::test]
    async fn sync_then_list_reflects_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let svc = services_over(&root, vec![spec("r1", VcsKind::Git)]);

        // before sync: no head recorded
        assert!(svc.list_repos()[0].head.is_none());

        let snap = svc.sync_repo(&RepoId::from("r1")).await.unwrap();
        assert_eq!(snap.head, "deadbeef");

        let info = svc.list_repos();
        assert_eq!(info[0].head.as_deref(), Some("deadbeef"));
        assert_eq!(info[0].file_count, Some(1));
    }

    #[tokio::test]
    async fn create_session_before_sync_is_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let svc = services_over(&root, vec![spec("r1", VcsKind::Git)]);
        let err = svc
            .create_session(&Principal::from("alice"), &RepoId::from("r1"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "conflict");
    }

    #[test]
    fn svn_without_backend_is_config_error() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let backends = BackendSet { git: Arc::new(FakeGit), svn: None };
        let err = RepoRegistry::build(vec![spec("r1", VcsKind::Svn)], &root, &backends)
            .err()
            .unwrap();
        assert_eq!(err.code(), "config_error");
    }

    #[tokio::test]
    async fn gc_keeps_current_and_referenced_reclaims_rest() {
        use std::sync::atomic::{AtomicU32, Ordering};

        // A backend whose head advances each sync, materializing snapshots/rev<n>.
        struct FakeGitSeq {
            n: AtomicU32,
        }
        #[async_trait]
        impl VcsBackend for FakeGitSeq {
            fn kind(&self) -> VcsKind {
                VcsKind::Git
            }
            async fn sync(&self, ctx: &RepoContext<'_>) -> Result<RepoSnapshot> {
                let n = self.n.fetch_add(1, Ordering::SeqCst) + 1;
                let head = format!("rev{n}");
                let base = ctx.snapshot_dir(&head);
                std::fs::create_dir_all(&base).unwrap();
                std::fs::write(base.join("f.txt"), b"x").unwrap();
                Ok(RepoSnapshot {
                    repo: ctx.spec.id.clone(),
                    head,
                    base_path: base,
                    synced_at: Timestamp(n as u64),
                    file_count: 1,
                })
            }
            async fn list_files(&self, _c: &RepoContext<'_>, _a: &Rev) -> Result<Vec<Utf8PathBuf>> {
                Ok(vec![])
            }
            async fn read_file(
                &self,
                _c: &RepoContext<'_>,
                p: &Utf8Path,
                _a: &Rev,
                r: Option<ReadRange>,
            ) -> Result<FileContent> {
                Ok(slice_file_content(p.to_owned(), b"", r))
            }
            async fn blame(&self, _c: &RepoContext<'_>, _p: &Utf8Path, _a: &Rev) -> Result<Vec<BlameLine>> {
                Ok(vec![])
            }
            async fn history(
                &self,
                _c: &RepoContext<'_>,
                _p: Option<&Utf8Path>,
                _q: &LogQuery,
            ) -> Result<Vec<Commit>> {
                Ok(vec![])
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let backends = BackendSet {
            git: Arc::new(FakeGitSeq { n: AtomicU32::new(0) }),
            svn: None,
        };
        let reg = Arc::new(RepoRegistry::build(vec![spec("r1", VcsKind::Git)], &root, &backends).unwrap());
        let sessions = Arc::new(NoSessions::default());
        let svc = Services::new(reg, Arc::new(NoSearch), sessions.clone(), Arc::new(NoAsm));

        // sync #1 -> snapshots/rev1 (current). Pretend a live session pins it.
        let snap1 = svc.sync_repo(&RepoId::from("r1")).await.unwrap();
        assert!(snap1.base_path.exists());
        sessions.bases.lock().unwrap().insert(snap1.base_path.clone());

        // sync #2 -> snapshots/rev2 (current). GC must keep BOTH rev2 (current)
        // and rev1 (still referenced by the session) — proving a re-sync never
        // reclaims a snapshot a session pinned.
        let snap2 = svc.sync_repo(&RepoId::from("r1")).await.unwrap();
        assert_ne!(snap1.base_path, snap2.base_path);
        assert!(snap1.base_path.exists(), "referenced snapshot kept");
        assert!(snap2.base_path.exists(), "current snapshot kept");

        // Session goes away; GC reclaims the now-unreferenced rev1, never rev2.
        sessions.bases.lock().unwrap().clear();
        svc.gc_all_snapshots().await;
        assert!(!snap1.base_path.exists(), "unreferenced old snapshot reclaimed");
        assert!(snap2.base_path.exists(), "current snapshot still kept");
    }

    #[test]
    fn duplicate_repo_id_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let backends = BackendSet { git: Arc::new(FakeGit), svn: None };
        let err = RepoRegistry::build(
            vec![spec("dup", VcsKind::Git), spec("dup", VcsKind::Git)],
            &root,
            &backends,
        )
        .err()
        .unwrap();
        assert_eq!(err.code(), "config_error");
    }
}

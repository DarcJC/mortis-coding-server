//! # mortis-session
//!
//! A disk-backed, copy-on-write [`SessionStore`](mortis_core::SessionStore).
//!
//! Each session is an isolated overlay on top of a repository's *read-only*
//! base working tree. Writes and deletes never touch the base: file content is
//! copied into a per-session `upper/` directory, and deletions of base files
//! are recorded as *whiteouts* in the session's metadata. Reads/search go
//! through an [`OverlayFileView`](mortis_fs::OverlayFileView), which unions the
//! upper layer and whiteouts over the base.
//!
//! ## On-disk layout
//!
//! ```text
//! <root>/
//!   <session-id>/
//!     meta.json        serialized `SessionRecord` (Session fields + whiteouts)
//!     upper/           copy-on-write file contents, mirroring base paths
//!       src/a.rs
//!       new.txt
//! ```
//!
//! Metadata is plain JSON (there is intentionally no database dependency).
//! Mutating operations are serialized through an in-process async mutex so that
//! concurrent writers cannot race the read-modify-write of `meta.json`; pure
//! reads (`get`/`list`/`status`/`diff`/`view`) run lock-free.

use std::collections::HashSet;
use std::time::Duration;

use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use similar::TextDiff;

use mortis_core::error::{CoreError, Result};
use mortis_core::model::{Principal, RepoId, SessionId, Timestamp};
use mortis_core::session::{ChangeKind, FileStatus, Session, SessionStore};
use mortis_core::view::FileView;
use mortis_fs::{OverlayFileView, PhysicalFileView};

/// Name of the per-session metadata file.
const META_FILE: &str = "meta.json";
/// Name of the per-session copy-on-write upper directory.
const UPPER_DIR: &str = "upper";

/// The persisted form of a [`Session`], extended with the whiteout set.
///
/// We store the whole [`Session`] inline plus `deleted` (paths of base files
/// that the session has removed). Kept private; converted to/from [`Session`]
/// at the API boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionRecord {
    #[serde(flatten)]
    session: Session,
    /// Base-relative paths whited-out by this session.
    #[serde(default)]
    deleted: Vec<Utf8PathBuf>,
}

impl SessionRecord {
    /// Build the whiteout set as a `HashSet` for overlay/lookup use.
    fn deleted_set(&self) -> HashSet<Utf8PathBuf> {
        self.deleted.iter().cloned().collect()
    }
}

/// A disk-backed copy-on-write session store.
///
/// `root` is the directory under which every session lives (one subdirectory
/// per session id). It is created on construction if missing.
#[derive(Debug)]
pub struct DiskSessionStore {
    /// The sessions directory (parent of all per-session subdirectories).
    root: Utf8PathBuf,
    /// Serializes mutating operations to protect `meta.json` integrity.
    write_lock: tokio::sync::Mutex<()>,
}

impl DiskSessionStore {
    /// Open (creating if necessary) a session store rooted at `root`.
    pub fn new(root: impl Into<Utf8PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            write_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// The directory holding a single session's data.
    fn session_dir(&self, id: &SessionId) -> Utf8PathBuf {
        self.root.join(&id.0)
    }

    /// The metadata file for a single session.
    fn meta_path(&self, id: &SessionId) -> Utf8PathBuf {
        self.session_dir(id).join(META_FILE)
    }

    /// The copy-on-write upper directory for a single session.
    fn upper_dir(&self, id: &SessionId) -> Utf8PathBuf {
        self.session_dir(id).join(UPPER_DIR)
    }

    /// Load a session record from disk, or [`CoreError::NotFound`] if absent.
    fn load_record(&self, id: &SessionId) -> Result<SessionRecord> {
        let path = self.meta_path(id);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(CoreError::not_found(id.0.clone()));
            }
            Err(e) => return Err(e.into()),
        };
        serde_json::from_slice(&bytes)
            .map_err(|e| CoreError::Session(format!("corrupt session metadata for {id}: {e}")))
    }

    /// Atomically persist a session record (write to a temp file then rename).
    ///
    /// The rename keeps `meta.json` from being observed half-written even if the
    /// process dies mid-write.
    fn store_record(&self, rec: &SessionRecord) -> Result<()> {
        let dir = self.session_dir(&rec.session.id);
        std::fs::create_dir_all(&dir)?;
        let json = serde_json::to_vec_pretty(rec)
            .map_err(|e| CoreError::Session(format!("failed to serialize session metadata: {e}")))?;
        let tmp = dir.join(format!("{META_FILE}.tmp"));
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, dir.join(META_FILE))?;
        Ok(())
    }
}

/// Reject paths that could escape the session's upper directory.
///
/// We forbid absolute paths and any `..` / root / prefix components, then
/// re-join the surviving normal components. The result is always a relative
/// path that stays inside `upper/`.
fn sanitize(rel: &Utf8Path) -> Result<Utf8PathBuf> {
    use camino::Utf8Component;

    if rel.as_str().is_empty() {
        return Err(CoreError::invalid("empty path"));
    }

    let mut out = Utf8PathBuf::new();
    for comp in rel.components() {
        match comp {
            Utf8Component::Normal(part) => out.push(part),
            // Reject anything that could walk out of, or anchor outside, upper/.
            Utf8Component::ParentDir
            | Utf8Component::RootDir
            | Utf8Component::Prefix(_) => {
                return Err(CoreError::invalid(format!(
                    "path escapes session root: {rel}"
                )));
            }
            // `.` is harmless; drop it.
            Utf8Component::CurDir => {}
        }
    }

    if out.as_str().is_empty() {
        return Err(CoreError::invalid(format!("path resolves to root: {rel}")));
    }
    // Normalize to forward slashes so logical paths match `mortis-fs` output
    // (which also normalizes) on every platform.
    Ok(Utf8PathBuf::from(out.as_str().replace('\\', "/")))
}

/// Recursively collect base-relative file paths under an upper directory.
fn collect_upper_files(upper: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    // The upper dir mirrors base paths, so a `PhysicalFileView` over it yields
    // exactly the logical (base-relative) paths the session has written.
    if !upper.exists() {
        return Ok(Vec::new());
    }
    PhysicalFileView::new(upper).list_files(None)
}

/// Read a logical file from a base view as bytes; empty if it does not exist.
fn read_or_empty(view: &PhysicalFileView, path: &Utf8Path) -> Vec<u8> {
    view.read(path).unwrap_or_default()
}

/// Render one file's git-style unified diff and append it to `out`.
///
/// `old`/`new` are the before/after contents (empty meaning "absent"). The
/// emitted block is `diff --git` + a `/dev/null`-aware unified diff, so the
/// concatenation of these blocks is a single git-apply-able patch.
fn append_file_diff(
    out: &mut String,
    path: &Utf8Path,
    change: ChangeKind,
    old: &[u8],
    new: &[u8],
) {
    let old_text = String::from_utf8_lossy(old);
    let new_text = String::from_utf8_lossy(new);

    // Patches must use forward slashes on every platform; `Utf8PathBuf` joins
    // with the OS separator (a backslash on Windows), so normalize for output.
    let slug = path.as_str().replace('\\', "/");

    // git headers: the `a/`/`b/` side of an add/delete is `/dev/null`.
    let (old_label, new_label) = match change {
        ChangeKind::Added => ("/dev/null".to_string(), format!("b/{slug}")),
        ChangeKind::Deleted => (format!("a/{slug}"), "/dev/null".to_string()),
        ChangeKind::Modified => (format!("a/{slug}"), format!("b/{slug}")),
    };

    let diff = TextDiff::from_lines(old_text.as_ref(), new_text.as_ref());
    let body = diff
        .unified_diff()
        .header(&old_label, &new_label)
        .to_string();

    // `diff --git` line always refers to the working paths (never /dev/null).
    out.push_str(&format!("diff --git a/{slug} b/{slug}\n"));
    out.push_str(&body);
    // Guard against missing trailing newline between consecutive file blocks.
    if !body.ends_with('\n') {
        out.push('\n');
    }
}

impl DiskSessionStore {
    /// Compute the status entries for a loaded record (shared by status/diff).
    ///
    /// Walks the upper layer (Added/Modified) and the whiteout set (Deleted),
    /// skipping no-op writes whose bytes equal the base. Result is sorted by
    /// path with a stable ordering.
    fn compute_status(&self, rec: &SessionRecord) -> Result<Vec<FileStatus>> {
        let base = PhysicalFileView::new(rec.session.base_path.clone());
        let upper = self.upper_dir(&rec.session.id);

        let mut entries: Vec<FileStatus> = Vec::new();

        // Upper layer: each written file is Added (no base) or Modified (base
        // present and bytes differ). Equal bytes are a no-op and skipped.
        for path in collect_upper_files(&upper)? {
            let upper_bytes = std::fs::read(upper.join(&path))?;
            match base.resolve(&path)? {
                Some(_) => {
                    let base_bytes = base.read(&path)?;
                    if base_bytes != upper_bytes {
                        entries.push(FileStatus {
                            path,
                            change: ChangeKind::Modified,
                        });
                    }
                }
                None => entries.push(FileStatus {
                    path,
                    change: ChangeKind::Added,
                }),
            }
        }

        // Whiteouts: report only those that actually shadow a base file.
        for path in &rec.deleted {
            if base.resolve(path)?.is_some() {
                entries.push(FileStatus {
                    path: path.clone(),
                    change: ChangeKind::Deleted,
                });
            }
        }

        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }
}

#[async_trait]
impl SessionStore for DiskSessionStore {
    async fn create(
        &self,
        owner: &Principal,
        repo: &RepoId,
        base_rev: &str,
        base_path: &Utf8Path,
    ) -> Result<Session> {
        let _guard = self.write_lock.lock().await;

        let now = Timestamp::now();
        let session = Session {
            id: SessionId(uuid::Uuid::new_v4().to_string()),
            owner: owner.clone(),
            repo: repo.clone(),
            base_rev: base_rev.to_owned(),
            base_path: base_path.to_owned(),
            created: now,
            last_accessed: now,
        };
        let rec = SessionRecord {
            session: session.clone(),
            deleted: Vec::new(),
        };

        // Materialize the layout: <id>/upper/ and <id>/meta.json.
        std::fs::create_dir_all(self.upper_dir(&session.id))?;
        self.store_record(&rec)?;

        Ok(session)
    }

    async fn get(&self, id: &SessionId) -> Result<Session> {
        Ok(self.load_record(id)?.session)
    }

    async fn list(&self, owner: &Principal) -> Result<Vec<Session>> {
        let mut out = Vec::new();
        let dir = match std::fs::read_dir(&self.root) {
            Ok(d) => d,
            // No sessions directory yet means no sessions.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };

        for entry in dir {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let id = SessionId(name.to_owned());
            // Tolerate stray/incomplete directories: skip anything unreadable.
            match self.load_record(&id) {
                Ok(rec) if rec.session.owner == *owner => out.push(rec.session),
                Ok(_) => {}
                Err(CoreError::NotFound(_)) => {}
                Err(e) => {
                    tracing::warn!(session = %id, error = %e, "skipping unreadable session");
                }
            }
        }

        out.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        Ok(out)
    }

    async fn delete(&self, id: &SessionId) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        let dir = self.session_dir(id);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(CoreError::not_found(id.0.clone()))
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn write_file(&self, id: &SessionId, path: &Utf8Path, content: &[u8]) -> Result<()> {
        let safe = sanitize(path)?;
        let _guard = self.write_lock.lock().await;

        let mut rec = self.load_record(id)?;
        let dest = self.upper_dir(id).join(&safe);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, content)?;

        // A write also "un-deletes": drop any matching whiteout.
        rec.deleted.retain(|p| p != &safe);
        rec.session.last_accessed = Timestamp::now();
        self.store_record(&rec)?;
        Ok(())
    }

    async fn delete_file(&self, id: &SessionId, path: &Utf8Path) -> Result<()> {
        let safe = sanitize(path)?;
        let _guard = self.write_lock.lock().await;

        let mut rec = self.load_record(id)?;
        let upper_path = self.upper_dir(id).join(&safe);
        let had_upper = upper_path.is_file();
        if had_upper {
            std::fs::remove_file(&upper_path)?;
        }

        // Whiteout only makes sense for files that exist in the base tree.
        let base = PhysicalFileView::new(rec.session.base_path.clone());
        let in_base = base.resolve(&safe)?.is_some();
        if in_base && !rec.deleted.contains(&safe) {
            rec.deleted.push(safe.clone());
        }

        // Nothing existed to delete in either layer.
        if !had_upper && !in_base {
            return Err(CoreError::not_found(safe.into_string()));
        }

        rec.session.last_accessed = Timestamp::now();
        self.store_record(&rec)?;
        Ok(())
    }

    async fn status(&self, id: &SessionId) -> Result<Vec<FileStatus>> {
        let rec = self.load_record(id)?;
        self.compute_status(&rec)
    }

    async fn diff(&self, id: &SessionId, path: Option<&Utf8Path>) -> Result<String> {
        let rec = self.load_record(id)?;
        let base = PhysicalFileView::new(rec.session.base_path.clone());
        let upper = self.upper_dir(id);

        // Single-file diff: classify the one path and render it.
        if let Some(p) = path {
            let safe = sanitize(p)?;
            let base_bytes = read_or_empty(&base, &safe);
            let upper_path = upper.join(&safe);
            let in_upper = upper_path.is_file();
            let in_base = base.resolve(&safe)?.is_some();
            let whited_out = rec.deleted.contains(&safe);

            let mut out = String::new();
            if in_upper {
                let new_bytes = std::fs::read(&upper_path)?;
                let change = if in_base {
                    ChangeKind::Modified
                } else {
                    ChangeKind::Added
                };
                // Skip no-op writes (bytes identical to base).
                if !(in_base && base_bytes == new_bytes) {
                    append_file_diff(&mut out, &safe, change, &base_bytes, &new_bytes);
                }
            } else if whited_out && in_base {
                append_file_diff(&mut out, &safe, ChangeKind::Deleted, &base_bytes, &[]);
            }
            return Ok(out);
        }

        // Whole-session diff: every change from `status`, in sorted order.
        let mut out = String::new();
        for st in self.compute_status(&rec)? {
            match st.change {
                ChangeKind::Added => {
                    let new_bytes = std::fs::read(upper.join(&st.path))?;
                    append_file_diff(&mut out, &st.path, st.change, &[], &new_bytes);
                }
                ChangeKind::Modified => {
                    let base_bytes = base.read(&st.path)?;
                    let new_bytes = std::fs::read(upper.join(&st.path))?;
                    append_file_diff(&mut out, &st.path, st.change, &base_bytes, &new_bytes);
                }
                ChangeKind::Deleted => {
                    let base_bytes = base.read(&st.path)?;
                    append_file_diff(&mut out, &st.path, st.change, &base_bytes, &[]);
                }
            }
        }
        Ok(out)
    }

    async fn export_patch(&self, id: &SessionId) -> Result<String> {
        // The export is exactly the whole-session diff.
        self.diff(id, None).await
    }

    async fn touch(&self, id: &SessionId) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        let mut rec = self.load_record(id)?;
        rec.session.last_accessed = Timestamp::now();
        self.store_record(&rec)?;
        Ok(())
    }

    async fn reap_expired(&self, ttl: Duration) -> Result<usize> {
        let _guard = self.write_lock.lock().await;

        let dir = match std::fs::read_dir(&self.root) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e.into()),
        };

        let now = Timestamp::now();
        let ttl_ms = ttl.as_millis() as u64;
        let mut reaped = 0usize;

        for entry in dir {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let id = SessionId(name.to_owned());
            let rec = match self.load_record(&id) {
                Ok(r) => r,
                Err(_) => continue,
            };

            // Idle duration since last access; saturating so clock skew can't
            // wrap into a huge value.
            let idle = now.0.saturating_sub(rec.session.last_accessed.0);
            if idle > ttl_ms {
                std::fs::remove_dir_all(self.session_dir(&id))?;
                reaped += 1;
            }
        }
        Ok(reaped)
    }

    async fn view(&self, id: &SessionId) -> Result<Box<dyn FileView>> {
        let rec = self.load_record(id)?;
        let view = OverlayFileView::new(
            rec.session.base_path.clone(),
            self.upper_dir(id),
            rec.deleted_set(),
        );
        Ok(Box::new(view))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Convert a `std::path::Path` to a `Utf8PathBuf` (test helper).
    fn u(p: &std::path::Path) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(p.to_path_buf()).unwrap()
    }

    /// A fixture: a temp dir holding `sessions/` (the store root) and a base
    /// repo tree with a couple of files.
    struct Fixture {
        _tmp: tempfile::TempDir,
        root: Utf8PathBuf,
        base: Utf8PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = u(&tmp.path().join("sessions"));
        let base = u(&tmp.path().join("base"));
        fs::create_dir_all(base.join("src")).unwrap();
        fs::write(base.join("src/a.rs"), b"base-a\n").unwrap();
        fs::write(base.join("keep.txt"), b"keep\n").unwrap();
        Fixture {
            _tmp: tmp,
            root,
            base,
        }
    }

    /// Create a store and a single session over the fixture base.
    async fn store_with_session(fx: &Fixture) -> (DiskSessionStore, Session) {
        let store = DiskSessionStore::new(fx.root.clone()).unwrap();
        let session = store
            .create(
                &Principal("alice".into()),
                &RepoId("repo-a".into()),
                "deadbeef",
                &fx.base,
            )
            .await
            .unwrap();
        (store, session)
    }

    #[tokio::test]
    async fn create_get_and_owner_isolation() {
        let fx = fixture();
        let store = DiskSessionStore::new(fx.root.clone()).unwrap();

        let alice = Principal("alice".into());
        let bob = Principal("bob".into());

        let s1 = store
            .create(&alice, &RepoId("repo-a".into()), "rev1", &fx.base)
            .await
            .unwrap();
        let _s2 = store
            .create(&bob, &RepoId("repo-a".into()), "rev1", &fx.base)
            .await
            .unwrap();

        // get returns the same session.
        let got = store.get(&s1.id).await.unwrap();
        assert_eq!(got.id, s1.id);
        assert_eq!(got.owner, alice);
        assert_eq!(got.base_rev, "rev1");

        // list is scoped to the owner: alice sees only her session.
        let alice_list = store.list(&alice).await.unwrap();
        assert_eq!(alice_list.len(), 1);
        assert_eq!(alice_list[0].id, s1.id);

        // bob sees only his (different) session.
        let bob_list = store.list(&bob).await.unwrap();
        assert_eq!(bob_list.len(), 1);
        assert_ne!(bob_list[0].id, s1.id);
    }

    #[tokio::test]
    async fn get_missing_is_not_found() {
        let fx = fixture();
        let store = DiskSessionStore::new(fx.root.clone()).unwrap();
        let err = store.get(&SessionId("nope".into())).await.unwrap_err();
        assert!(matches!(err, CoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn write_modifies_overlay_not_base() {
        let fx = fixture();
        let (store, s) = store_with_session(&fx).await;

        store
            .write_file(&s.id, Utf8Path::new("src/a.rs"), b"changed-a\n")
            .await
            .unwrap();

        // status reports the modification.
        let status = store.status(&s.id).await.unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].path, Utf8PathBuf::from("src/a.rs"));
        assert_eq!(status[0].change, ChangeKind::Modified);

        // view().read() returns the NEW bytes.
        let view = store.view(&s.id).await.unwrap();
        assert_eq!(view.read(Utf8Path::new("src/a.rs")).unwrap(), b"changed-a\n");

        // ...while the base file on disk is UNCHANGED.
        let base_bytes = fs::read(fx.base.join("src/a.rs")).unwrap();
        assert_eq!(base_bytes, b"base-a\n");
    }

    #[tokio::test]
    async fn add_new_file_is_added() {
        let fx = fixture();
        let (store, s) = store_with_session(&fx).await;

        store
            .write_file(&s.id, Utf8Path::new("src/new.rs"), b"brand new\n")
            .await
            .unwrap();

        let status = store.status(&s.id).await.unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].path, Utf8PathBuf::from("src/new.rs"));
        assert_eq!(status[0].change, ChangeKind::Added);

        // The base never gains the file.
        assert!(!fx.base.join("src/new.rs").exists());
    }

    #[tokio::test]
    async fn delete_base_file_whiteouts_it() {
        let fx = fixture();
        let (store, s) = store_with_session(&fx).await;

        store
            .delete_file(&s.id, Utf8Path::new("keep.txt"))
            .await
            .unwrap();

        // status reports the deletion.
        let status = store.status(&s.id).await.unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].path, Utf8PathBuf::from("keep.txt"));
        assert_eq!(status[0].change, ChangeKind::Deleted);

        // view no longer lists or resolves the file...
        let view = store.view(&s.id).await.unwrap();
        let files = view.list_files(None).unwrap();
        assert!(!files.contains(&Utf8PathBuf::from("keep.txt")));
        assert!(view.resolve(Utf8Path::new("keep.txt")).unwrap().is_none());

        // ...but the base file on disk is still there.
        assert!(fx.base.join("keep.txt").exists());
    }

    #[tokio::test]
    async fn write_then_delete_then_status() {
        let fx = fixture();
        let (store, s) = store_with_session(&fx).await;

        // Delete an upper-only added file: removes it, no whiteout, no status.
        store
            .write_file(&s.id, Utf8Path::new("temp.txt"), b"tmp\n")
            .await
            .unwrap();
        store
            .delete_file(&s.id, Utf8Path::new("temp.txt"))
            .await
            .unwrap();
        let status = store.status(&s.id).await.unwrap();
        assert!(status.is_empty(), "added-then-deleted leaves no trace");
    }

    #[tokio::test]
    async fn deleting_nonexistent_is_not_found() {
        let fx = fixture();
        let (store, s) = store_with_session(&fx).await;
        let err = store
            .delete_file(&s.id, Utf8Path::new("does/not/exist.txt"))
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn write_undeletes_a_whiteout() {
        let fx = fixture();
        let (store, s) = store_with_session(&fx).await;

        store
            .delete_file(&s.id, Utf8Path::new("keep.txt"))
            .await
            .unwrap();
        // Re-writing the path resurrects it (and changes its content).
        store
            .write_file(&s.id, Utf8Path::new("keep.txt"), b"resurrected\n")
            .await
            .unwrap();

        let status = store.status(&s.id).await.unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].change, ChangeKind::Modified);

        let view = store.view(&s.id).await.unwrap();
        assert_eq!(view.read(Utf8Path::new("keep.txt")).unwrap(), b"resurrected\n");
    }

    #[tokio::test]
    async fn noop_write_is_not_reported() {
        let fx = fixture();
        let (store, s) = store_with_session(&fx).await;

        // Writing the SAME bytes the base already has is a no-op for status.
        store
            .write_file(&s.id, Utf8Path::new("src/a.rs"), b"base-a\n")
            .await
            .unwrap();

        let status = store.status(&s.id).await.unwrap();
        assert!(status.is_empty(), "identical content must not show as changed");
    }

    #[tokio::test]
    async fn diff_and_patch_are_git_applyable() {
        let fx = fixture();
        let (store, s) = store_with_session(&fx).await;

        store
            .write_file(&s.id, Utf8Path::new("src/a.rs"), b"changed-a\n")
            .await
            .unwrap();
        store
            .write_file(&s.id, Utf8Path::new("added.txt"), b"new file\n")
            .await
            .unwrap();
        store
            .delete_file(&s.id, Utf8Path::new("keep.txt"))
            .await
            .unwrap();

        let patch = store.export_patch(&s.id).await.unwrap();

        // Contains git diff headers and hunk markers.
        assert!(patch.contains("diff --git a/src/a.rs b/src/a.rs"), "{patch}");
        assert!(patch.contains("@@"), "{patch}");
        // Added file: /dev/null on the old side.
        assert!(patch.contains("diff --git a/added.txt b/added.txt"), "{patch}");
        assert!(patch.contains("--- /dev/null"), "{patch}");
        // Deleted file: /dev/null on the new side.
        assert!(patch.contains("+++ /dev/null"), "{patch}");
        // Actual content lines present.
        assert!(patch.contains("+changed-a"), "{patch}");
        assert!(patch.contains("-base-a"), "{patch}");

        // A single-file diff matches the corresponding block.
        let one = store
            .diff(&s.id, Some(Utf8Path::new("src/a.rs")))
            .await
            .unwrap();
        assert!(one.contains("diff --git a/src/a.rs b/src/a.rs"), "{one}");
        assert!(one.contains("+changed-a"), "{one}");
        assert!(!one.contains("added.txt"), "single-file diff is scoped");

        // export_patch equals the whole-session diff.
        let whole = store.diff(&s.id, None).await.unwrap();
        assert_eq!(patch, whole);
    }

    #[tokio::test]
    async fn diff_of_unchanged_file_is_empty() {
        let fx = fixture();
        let (store, s) = store_with_session(&fx).await;
        // No writes: a single-file diff against an unchanged base file is empty.
        let d = store
            .diff(&s.id, Some(Utf8Path::new("src/a.rs")))
            .await
            .unwrap();
        assert!(d.is_empty(), "unchanged file should produce no diff: {d:?}");
    }

    #[tokio::test]
    async fn persistence_across_reopen() {
        let fx = fixture();
        let session_id;
        {
            let (store, s) = store_with_session(&fx).await;
            session_id = s.id.clone();
            store
                .write_file(&s.id, Utf8Path::new("src/a.rs"), b"persisted\n")
                .await
                .unwrap();
            store
                .delete_file(&s.id, Utf8Path::new("keep.txt"))
                .await
                .unwrap();
            // store dropped here.
        }

        // Reopen over the same root: state survives.
        let store2 = DiskSessionStore::new(fx.root.clone()).unwrap();
        let got = store2.get(&session_id).await.unwrap();
        assert_eq!(got.base_rev, "deadbeef");

        let view = store2.view(&session_id).await.unwrap();
        assert_eq!(view.read(Utf8Path::new("src/a.rs")).unwrap(), b"persisted\n");
        // Whiteout survived too.
        assert!(view.resolve(Utf8Path::new("keep.txt")).unwrap().is_none());

        // And status is rebuilt correctly from disk.
        let status = store2.status(&session_id).await.unwrap();
        assert_eq!(status.len(), 2);
    }

    #[tokio::test]
    async fn touch_updates_last_accessed() {
        let fx = fixture();
        let (store, s) = store_with_session(&fx).await;
        let before = store.get(&s.id).await.unwrap().last_accessed;
        // Force a measurable clock tick.
        std::thread::sleep(std::time::Duration::from_millis(5));
        store.touch(&s.id).await.unwrap();
        let after = store.get(&s.id).await.unwrap().last_accessed;
        assert!(after >= before);
    }

    #[tokio::test]
    async fn reap_expired_removes_idle_sessions() {
        let fx = fixture();
        let (store, s) = store_with_session(&fx).await;

        // With ZERO ttl, any positive idle time is expired. Sleep a touch so
        // `now - last_accessed > 0`.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let reaped = store.reap_expired(Duration::ZERO).await.unwrap();
        assert_eq!(reaped, 1);

        // The session and its directory are gone.
        assert!(matches!(
            store.get(&s.id).await.unwrap_err(),
            CoreError::NotFound(_)
        ));
        assert!(!fx.root.join(&s.id.0).exists());
    }

    #[tokio::test]
    async fn reap_keeps_fresh_sessions() {
        let fx = fixture();
        let (store, s) = store_with_session(&fx).await;
        // A large ttl keeps a freshly-created session.
        let reaped = store.reap_expired(Duration::from_secs(3600)).await.unwrap();
        assert_eq!(reaped, 0);
        assert!(store.get(&s.id).await.is_ok());
    }

    #[tokio::test]
    async fn path_traversal_is_rejected() {
        let fx = fixture();
        let (store, s) = store_with_session(&fx).await;

        for bad in ["../escape.txt", "a/../../b.txt", "/abs.txt"] {
            let err = store
                .write_file(&s.id, Utf8Path::new(bad), b"x")
                .await
                .unwrap_err();
            assert!(
                matches!(err, CoreError::InvalidInput(_)),
                "expected rejection for {bad:?}, got {err:?}"
            );
        }
    }
}

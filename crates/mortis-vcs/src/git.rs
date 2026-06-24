//! Read-only Git backend powered by `gitoxide` (`gix`).
//!
//! Pure Rust: no system `git`, no C toolchain. Network transport is HTTPS via
//! `reqwest` + `rustls`. The backend never checks out a full worktree — instead
//! it fetches into a bare object store under `<root>/vcs` and *materializes*
//! only the whitelisted paths of the head tree into an immutable, per-revision
//! snapshot `<root>/snapshots/<head>`, which is the read-only base that search
//! and sessions sit on. A re-sync to a new head publishes a new snapshot dir
//! rather than mutating an existing one, so a live session's base is stable.
//!
//! Snapshots are keyed by resolved head only; within a process the whitelist is
//! constant so the head fully determines content. If an operator edits
//! `include`/`exclude`, restarts, and the head is unchanged, an existing
//! snapshot is reused (stale-whitelist, never corrupt) — bump `rev` or delete
//! the snapshot dir to force a rebuild.
//!
//! All `gix` work is blocking, so every trait method moves the work onto a
//! blocking task and (re)opens the repository inside it — this sidesteps any
//! `Send` constraints on `gix::Repository` across `.await` points.

use std::sync::atomic::AtomicBool;

use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use gix::ObjectId;
use gix::bstr::{BStr, ByteSlice};
use std::collections::HashMap;

use mortis_core::vcs::RepoContext;
use mortis_core::{
    BlameLine, Commit, CoreError, FileContent, LogQuery, ReadRange, RepoSnapshot, Result, Rev,
    Timestamp, VcsBackend, VcsKind, slice_file_content,
};

use crate::filter::GlobFilter;

/// The standard refspec used to mirror remote branches into tracking refs.
const MIRROR_REFSPEC: &str = "+refs/heads/*:refs/remotes/origin/*";

/// A read-only Git backend backed by `gitoxide`.
#[derive(Debug, Default, Clone)]
pub struct GixBackend;

impl GixBackend {
    pub fn new() -> Self {
        GixBackend
    }
}

/// Shorthand for mapping any displayable error into [`CoreError::Vcs`].
fn vcs<E: std::fmt::Display>(e: E) -> CoreError {
    CoreError::Vcs(e.to_string())
}

#[async_trait]
impl VcsBackend for GixBackend {
    fn kind(&self) -> VcsKind {
        VcsKind::Git
    }

    async fn sync(&self, ctx: &RepoContext<'_>) -> Result<RepoSnapshot> {
        let internal = ctx.internal_dir();
        let snapshots_dir = ctx.snapshots_dir();
        let url = ctx.spec.url.clone();
        let spec_rev = ctx.spec.rev.clone();
        let include = ctx.spec.include.clone();
        let exclude = ctx.spec.exclude.clone();
        let repo_id = ctx.spec.id.clone();

        blocking(move || {
            std::fs::create_dir_all(&internal)?;

            // Open the bare object store, or create it on first sync.
            let repo = if internal.join("HEAD").exists() {
                gix::open(internal.as_std_path()).map_err(vcs)?
            } else {
                gix::init_bare(internal.as_std_path()).map_err(vcs)?
            };

            // Fetch from the remote into refs/remotes/origin/*.
            let interrupt = AtomicBool::new(false);
            let remote = repo
                .remote_at(url.as_str())
                .map_err(vcs)?
                .with_refspecs([MIRROR_REFSPEC], gix::remote::Direction::Fetch)
                .map_err(vcs)?;
            remote
                .connect(gix::remote::Direction::Fetch)
                .map_err(vcs)?
                .prepare_fetch(gix::progress::Discard, Default::default())
                .map_err(vcs)?
                .receive(gix::progress::Discard, &interrupt)
                .map_err(vcs)?;

            // Resolve the head and materialize its whitelisted tree into an
            // immutable, per-revision snapshot directory (published atomically;
            // an existing snapshot for this head is reused).
            let head = resolve_commit(&repo, &Rev::Head, spec_rev.as_deref())?;
            let head_hex = head.to_hex().to_string();
            let commit = repo.find_object(head).map_err(vcs)?.try_into_commit().map_err(vcs)?;
            let tree = commit.tree().map_err(vcs)?;
            let filter = GlobFilter::new(&include, &exclude)?;

            let (base_path, count) =
                crate::publish::publish_snapshot(&snapshots_dir, &head_hex, |staging| {
                    let mut count = 0usize;
                    materialize_tree(&tree, "", staging, &filter, &mut count)?;
                    Ok(count)
                })?;

            Ok(RepoSnapshot {
                repo: repo_id,
                head: head_hex,
                base_path,
                synced_at: Timestamp::now(),
                file_count: count,
            })
        })
        .await
    }

    async fn list_files(&self, ctx: &RepoContext<'_>, at: &Rev) -> Result<Vec<Utf8PathBuf>> {
        let internal = ctx.internal_dir();
        let at = at.clone();
        let spec_rev = ctx.spec.rev.clone();
        blocking(move || {
            let repo = gix::open(internal.as_std_path()).map_err(vcs)?;
            let id = resolve_commit(&repo, &at, spec_rev.as_deref())?;
            let tree = repo.find_object(id).map_err(vcs)?.try_into_commit().map_err(vcs)?.tree().map_err(vcs)?;
            let mut out = Vec::new();
            collect_paths(&tree, "", &mut out)?;
            out.sort();
            Ok(out)
        })
        .await
    }

    async fn read_file(
        &self,
        ctx: &RepoContext<'_>,
        path: &Utf8Path,
        at: &Rev,
        range: Option<ReadRange>,
    ) -> Result<FileContent> {
        let internal = ctx.internal_dir();
        let at = at.clone();
        let spec_rev = ctx.spec.rev.clone();
        let path = path.to_owned();
        blocking(move || {
            let repo = gix::open(internal.as_std_path()).map_err(vcs)?;
            let id = resolve_commit(&repo, &at, spec_rev.as_deref())?;
            let tree = repo.find_object(id).map_err(vcs)?.try_into_commit().map_err(vcs)?.tree().map_err(vcs)?;
            let entry = tree
                .lookup_entry_by_path(path.as_std_path())
                .map_err(vcs)?
                .ok_or_else(|| CoreError::not_found(path.as_str()))?;
            let obj = entry.object().map_err(vcs)?;
            Ok(slice_file_content(path, &obj.data, range))
        })
        .await
    }

    async fn blame(&self, ctx: &RepoContext<'_>, path: &Utf8Path, at: &Rev) -> Result<Vec<BlameLine>> {
        let internal = ctx.internal_dir();
        let at = at.clone();
        let spec_rev = ctx.spec.rev.clone();
        let path = path.to_owned();
        blocking(move || {
            let repo = gix::open(internal.as_std_path()).map_err(vcs)?;
            let commit = resolve_commit(&repo, &at, spec_rev.as_deref())?;
            let outcome = repo
                .blame_file(
                    BStr::new(path.as_str()),
                    commit,
                    gix::repository::blame_file::Options::default(),
                )
                .map_err(vcs)?;

            let mut commit_cache: HashMap<ObjectId, CommitMeta> = HashMap::new();
            let mut lines = Vec::new();
            for (entry, entry_lines) in outcome.entries_with_lines() {
                let meta = match commit_cache.get(&entry.commit_id) {
                    Some(m) => m.clone(),
                    None => {
                        let m = commit_meta(&repo, entry.commit_id)?;
                        commit_cache.insert(entry.commit_id, m.clone());
                        m
                    }
                };
                for (i, line) in entry_lines.into_iter().enumerate() {
                    lines.push(BlameLine {
                        line_no: entry
                            .start_in_blamed_file
                            .saturating_add(i as u32)
                            .saturating_add(1),
                        commit: meta.id.clone(),
                        author: meta.author.clone(),
                        author_email: meta.email.clone(),
                        time: meta.time,
                        summary: meta.summary.clone(),
                        content: line.to_str_lossy().into_owned(),
                    });
                }
            }
            Ok(lines)
        })
        .await
    }

    async fn history(
        &self,
        ctx: &RepoContext<'_>,
        path: Option<&Utf8Path>,
        query: &LogQuery,
    ) -> Result<Vec<Commit>> {
        let internal = ctx.internal_dir();
        let spec_rev = ctx.spec.rev.clone();
        let path = path.map(|p| p.to_owned());
        let query = query.clone();
        blocking(move || {
            let repo = gix::open(internal.as_std_path()).map_err(vcs)?;
            let tip = resolve_commit(&repo, &Rev::Head, spec_rev.as_deref())?;
            let walk = repo.rev_walk(Some(tip)).all().map_err(vcs)?;

            let skip = query.skip.unwrap_or(0);
            let limit = query.max_count.unwrap_or(usize::MAX);
            let mut out = Vec::new();
            let mut seen = 0usize;

            for info in walk {
                let info = info.map_err(vcs)?;
                let id = info.id;
                // For per-file history, keep only commits that changed `path`.
                if let Some(p) = &path {
                    if !commit_touches_path(&repo, id, &info, p)? {
                        continue;
                    }
                }
                if seen < skip {
                    seen += 1;
                    continue;
                }
                out.push(commit_meta(&repo, id)?.into_commit());
                if out.len() >= limit {
                    break;
                }
            }
            Ok(out)
        })
        .await
    }
}

/// Run blocking `gix` work on the blocking pool, flattening the join error.
async fn blocking<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| CoreError::Other(format!("blocking task failed: {e}")))?
}

/// Resolve a [`Rev`] to a concrete commit, trying remote-tracking refs first.
fn resolve_commit(repo: &gix::Repository, at: &Rev, spec_rev: Option<&str>) -> Result<ObjectId> {
    let mut candidates: Vec<String> = Vec::new();
    match at {
        Rev::At(s) => {
            candidates.push(s.clone());
            candidates.push(format!("origin/{s}"));
            candidates.push(format!("refs/remotes/origin/{s}"));
        }
        Rev::Head => {
            if let Some(s) = spec_rev {
                candidates.push(s.to_string());
                candidates.push(format!("origin/{s}"));
                candidates.push(format!("refs/remotes/origin/{s}"));
            }
            candidates.extend([
                "origin/HEAD".into(),
                "origin/main".into(),
                "origin/master".into(),
                "HEAD".into(),
            ]);
        }
    }
    for cand in &candidates {
        if let Ok(id) = repo.rev_parse_single(BStr::new(cand.as_str())) {
            return Ok(id.detach());
        }
    }
    Err(CoreError::not_found(format!(
        "could not resolve revision (tried: {})",
        candidates.join(", ")
    )))
}

/// Recursively materialize whitelisted blobs of `tree` into `work`.
fn materialize_tree(
    tree: &gix::Tree<'_>,
    prefix: &str,
    work: &Utf8Path,
    filter: &GlobFilter,
    count: &mut usize,
) -> Result<()> {
    for entry in tree.iter() {
        let entry = entry.map_err(vcs)?;
        let name = entry
            .filename()
            .to_str()
            .map_err(|_| CoreError::Vcs("non-utf8 path component".into()))?;
        let rel = join_rel(prefix, name);
        let mode = entry.mode();
        if mode.is_tree() {
            let subtree = entry.object().map_err(vcs)?.into_tree();
            materialize_tree(&subtree, &rel, work, filter, count)?;
        } else if mode.is_blob() && filter.matches(Utf8Path::new(&rel)) {
            let obj = entry.object().map_err(vcs)?;
            let dest = work.join(&rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, &obj.data)?;
            *count += 1;
        }
    }
    Ok(())
}

/// Recursively collect all blob paths of `tree` (forward-slash, repo-relative).
fn collect_paths(tree: &gix::Tree<'_>, prefix: &str, out: &mut Vec<Utf8PathBuf>) -> Result<()> {
    for entry in tree.iter() {
        let entry = entry.map_err(vcs)?;
        let name = entry
            .filename()
            .to_str()
            .map_err(|_| CoreError::Vcs("non-utf8 path component".into()))?;
        let rel = join_rel(prefix, name);
        let mode = entry.mode();
        if mode.is_tree() {
            let subtree = entry.object().map_err(vcs)?.into_tree();
            collect_paths(&subtree, &rel, out)?;
        } else if mode.is_blob() {
            out.push(Utf8PathBuf::from(rel));
        }
    }
    Ok(())
}

/// Join a forward-slash prefix and a component.
fn join_rel(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

/// The blob id of `path` at `commit`, or `None` if absent.
fn blob_oid_at(repo: &gix::Repository, commit: ObjectId, path: &Utf8Path) -> Result<Option<ObjectId>> {
    let tree = repo
        .find_object(commit)
        .map_err(vcs)?
        .try_into_commit()
        .map_err(vcs)?
        .tree()
        .map_err(vcs)?;
    Ok(tree
        .lookup_entry_by_path(path.as_std_path())
        .map_err(vcs)?
        .map(|e| e.oid().to_owned()))
}

/// Whether `commit` changed `path` relative to its first parent (git log -- path).
fn commit_touches_path(
    repo: &gix::Repository,
    commit: ObjectId,
    info: &gix::revision::walk::Info<'_>,
    path: &Utf8Path,
) -> Result<bool> {
    let here = blob_oid_at(repo, commit, path)?;
    let mut parents = info.parent_ids.iter().copied();
    match parents.next() {
        // Root commit: it "touches" the path iff the path exists there.
        None => Ok(here.is_some()),
        Some(parent) => {
            let there = blob_oid_at(repo, parent, path)?;
            Ok(here != there)
        }
    }
}

/// Cached, owned commit metadata (so blame can dedupe per commit).
#[derive(Clone)]
struct CommitMeta {
    id: String,
    author: String,
    email: String,
    time: Timestamp,
    summary: String,
    message: String,
    parents: Vec<String>,
}

impl CommitMeta {
    fn into_commit(self) -> Commit {
        Commit {
            id: self.id,
            author: self.author,
            author_email: self.email,
            time: self.time,
            summary: self.summary,
            message: self.message,
            parents: self.parents,
        }
    }
}

/// Load and decode commit metadata.
fn commit_meta(repo: &gix::Repository, id: ObjectId) -> Result<CommitMeta> {
    let commit = repo.find_object(id).map_err(vcs)?.try_into_commit().map_err(vcs)?;
    let author = commit.author().map_err(vcs)?;
    let message = commit
        .message_raw()
        .map_err(vcs)?
        .to_str_lossy()
        .into_owned();
    let summary = message.lines().next().unwrap_or("").to_string();
    let parents = commit.parent_ids().map(|p| p.detach().to_hex().to_string()).collect();
    Ok(CommitMeta {
        id: id.to_hex().to_string(),
        author: author.name.to_str_lossy().into_owned(),
        email: author.email.to_str_lossy().into_owned(),
        time: Timestamp::from_unix_secs(author.seconds()),
        summary,
        message,
        parents,
    })
}

//! Atomic publication of an immutable per-revision snapshot directory.
//!
//! Shared by the Git and SVN backends so they don't duplicate the
//! staging → rename → reuse dance. The two backends differ only in *how* they
//! fill the staging directory (a `gix` tree walk vs an `svn export` + copy);
//! everything around that is identical and lives here.

use camino::{Utf8Path, Utf8PathBuf};

use mortis_core::{CoreError, RepoId, RepoSnapshot, Result, Timestamp};

/// Materialize a snapshot for `head` under `snapshots_dir` and publish it
/// atomically, returning `(base_path, file_count)`.
///
/// - If `snapshots_dir/<head>` already exists it is **reused** (an idempotent
///   re-sync of the same head): published snapshots are immutable, so any
///   session pinning it keeps seeing identical content. The file count is
///   recomputed by walking the tree so `RepoSnapshot.file_count` stays accurate
///   even across a restart that reuses an on-disk snapshot.
/// - Otherwise `fill` materializes into a staging directory under the same
///   parent and the result is renamed into place (rename within a directory is
///   atomic on every platform).
///
/// Callers must serialize syncs of the same repo (see the per-repo sync lock in
/// `mortis-app`) so the per-head staging directory has a single writer; the
/// lost-race branch below is a defensive guard for the unusual multi-process
/// case where two processes share one data directory.
pub(crate) fn publish_snapshot<F>(
    snapshots_dir: &Utf8Path,
    head: &str,
    fill: F,
) -> Result<(Utf8PathBuf, usize)>
where
    F: FnOnce(&Utf8Path) -> Result<usize>,
{
    std::fs::create_dir_all(snapshots_dir)?;
    let target = snapshots_dir.join(head);

    if target.exists() {
        let count = count_files(&target)?;
        return Ok((target, count));
    }

    // Stage under the same parent so the publish is an atomic rename. Pre-clean
    // any leftover from a previously crashed attempt.
    let staging = snapshots_dir.join(format!(".staging-{head}"));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;

    let count = match fill(&staging) {
        Ok(c) => c,
        Err(e) => {
            std::fs::remove_dir_all(&staging).ok();
            return Err(e);
        }
    };

    match std::fs::rename(&staging, &target) {
        Ok(()) => Ok((target, count)),
        // A concurrent publisher of the same head won the race; adopt its tree.
        Err(_) if target.exists() => {
            std::fs::remove_dir_all(&staging).ok();
            let count = count_files(&target)?;
            Ok((target, count))
        }
        Err(e) => {
            std::fs::remove_dir_all(&staging).ok();
            Err(e.into())
        }
    }
}

/// Recover the newest published snapshot under `snapshots_dir` as a
/// [`RepoSnapshot`], WITHOUT any network access. Returns `Ok(None)` when the
/// directory is absent or holds no published snapshot.
///
/// Shared by the Git and SVN backends' `rehydrate`. Selection is by greatest
/// directory mtime — a freshly published head is renamed into place last, so it
/// has the newest mtime. The directory name only breaks exact mtime ties, for
/// determinism; it is NOT a recency signal (head names are commit SHAs / svn
/// revnums, whose lexical order is unrelated to time). In-progress publishes
/// (`.staging-*`) are skipped. Best-effort: after a normal sync the post-sync
/// GC leaves only the current head plus any session-pinned dirs, so this
/// usually has a single candidate; whatever it picks, the next `sync`
/// re-resolves the true head and replaces it.
pub(crate) fn rehydrate_snapshot(
    repo: RepoId,
    snapshots_dir: &Utf8Path,
) -> Result<Option<RepoSnapshot>> {
    let Ok(read) = std::fs::read_dir(snapshots_dir) else {
        return Ok(None); // never published (or unreadable) → nothing to rehydrate
    };

    // Published snapshot dirs only: real subdirectories, excluding in-progress
    // `.staging-*` publishes and non-UTF-8 names (never a head we wrote).
    let newest = read
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let path = Utf8PathBuf::from_path_buf(e.path()).ok()?;
            let name = path.file_name()?;
            if name.starts_with(".staging-") {
                return None;
            }
            let mtime = e.metadata().and_then(|m| m.modified()).ok();
            Some((mtime, name.to_owned(), path))
        })
        .max_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));

    let Some((mtime, head, base_path)) = newest else {
        return Ok(None);
    };
    let file_count = count_files(&base_path)?;
    Ok(Some(RepoSnapshot {
        repo,
        head,
        base_path,
        // The dir's own mtime ≈ when it was published — far truer than `now()`
        // for a tree materialized in a previous process run.
        synced_at: mtime.map(Timestamp::from_system).unwrap_or_else(Timestamp::now),
        file_count,
    }))
}

/// Run [`rehydrate_snapshot`] on the blocking pool. Shared by the Git and SVN
/// `rehydrate` overrides so the offload + join-error mapping live in one place.
pub(crate) async fn rehydrate_offloaded(
    repo: RepoId,
    snapshots_dir: Utf8PathBuf,
) -> Result<Option<RepoSnapshot>> {
    tokio::task::spawn_blocking(move || rehydrate_snapshot(repo, &snapshots_dir))
        .await
        .map_err(|e| CoreError::Other(format!("blocking task failed: {e}")))?
}

/// Count regular files under `dir`, recursively (matches what the backends'
/// materialize/copy steps count).
fn count_files(dir: &Utf8Path) -> Result<usize> {
    let mut count = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            let sub = Utf8PathBuf::from_path_buf(entry.path())
                .map_err(|p| CoreError::Vcs(format!("non-utf8 path: {}", p.display())))?;
            count += count_files(&sub)?;
        } else if ft.is_file() {
            count += 1;
        }
    }
    Ok(count)
}

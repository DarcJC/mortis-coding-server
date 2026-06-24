//! Atomic publication of an immutable per-revision snapshot directory.
//!
//! Shared by the Git and SVN backends so they don't duplicate the
//! staging → rename → reuse dance. The two backends differ only in *how* they
//! fill the staging directory (a `gix` tree walk vs an `svn export` + copy);
//! everything around that is identical and lives here.

use camino::{Utf8Path, Utf8PathBuf};

use mortis_core::{CoreError, Result};

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

//! Tests for `VcsBackend::rehydrate` — recovering the newest on-disk snapshot
//! without any network access.
//!
//! These build snapshot directories by hand (no `git`/`svn` needed) and call
//! `rehydrate` directly, so they exercise the shared selection + counting logic
//! in isolation from a real `sync`.

use camino::Utf8PathBuf;

use mortis_core::config::{RepoConfig, VcsKind};
use mortis_core::vcs::RepoContext;
use mortis_core::{RepoId, VcsBackend};
use mortis_vcs::GixBackend;

fn u(p: &std::path::Path) -> Utf8PathBuf {
    Utf8PathBuf::from_path_buf(p.to_path_buf()).unwrap()
}

fn spec() -> RepoConfig {
    RepoConfig {
        id: RepoId::from("proj"),
        kind: VcsKind::Git,
        url: "file:///x".into(),
        rev: None,
        schedule: None,
        include: vec![],
        exclude: vec![],
        username: None,
        password: None,
    }
}

/// Write a fake published snapshot at `<data>/snapshots/<head>/...`.
fn write_snapshot(ctx: &RepoContext<'_>, head: &str, files: &[(&str, &str)]) {
    let dir = ctx.snapshot_dir(head);
    for (rel, content) in files {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
}

#[tokio::test]
async fn rehydrate_returns_published_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let data = u(tmp.path());
    let spec = spec();
    let ctx = RepoContext::new(&spec, &data);

    write_snapshot(&ctx, "abc123", &[("src/a.rs", "fn a() {}\n"), ("README.md", "# hi\n")]);

    let snap = GixBackend::new().rehydrate(&ctx).await.unwrap().expect("a snapshot");
    assert_eq!(snap.head, "abc123");
    assert_eq!(snap.base_path, ctx.snapshot_dir("abc123"));
    assert_eq!(snap.file_count, 2);
    assert!(snap.base_path.join("src/a.rs").exists());
}

#[tokio::test]
async fn rehydrate_on_empty_data_dir_is_none() {
    let tmp = tempfile::tempdir().unwrap();
    let data = u(tmp.path());
    let spec = spec();
    let ctx = RepoContext::new(&spec, &data);
    // No snapshots/ directory at all → nothing to rehydrate.
    assert!(GixBackend::new().rehydrate(&ctx).await.unwrap().is_none());
}

#[tokio::test]
async fn rehydrate_skips_staging_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let data = u(tmp.path());
    let spec = spec();
    let ctx = RepoContext::new(&spec, &data);

    // ONLY an in-progress publish dir exists. It must never be adopted as a
    // head, so rehydrate finds nothing. (This is a tight test: if the
    // `.staging-` filter regressed, this dir would be selected and rehydrate
    // would wrongly return Some — a real-head-also-present variant could pass
    // vacuously when mtimes tie and the name tiebreak happens to favor it.)
    let staging = ctx.snapshots_dir().join(".staging-deadbeef");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("partial.txt"), "y\n").unwrap();

    assert!(GixBackend::new().rehydrate(&ctx).await.unwrap().is_none());
}

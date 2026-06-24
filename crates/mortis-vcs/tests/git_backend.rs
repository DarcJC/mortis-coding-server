//! Integration tests for the gix Git backend.
//!
//! These build a throwaway Git repository with the system `git` (a *test-only*
//! convenience — the backend itself never shells out to git) and then exercise
//! sync + whitelist materialization, ranged reads, blame, and history against
//! it over a `file://` URL.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};

use mortis_core::config::{RepoConfig, VcsKind};
use mortis_core::vcs::RepoContext;
use mortis_core::{LogQuery, ReadRange, RepoId, Rev, VcsBackend};
use mortis_vcs::GixBackend;

fn u(p: &std::path::Path) -> Utf8PathBuf {
    Utf8PathBuf::from_path_buf(p.to_path_buf()).unwrap()
}

/// Run `git` in `dir` with deterministic identity, asserting success.
fn git(dir: &Utf8Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .expect("failed to spawn git");
    assert!(status.success(), "git {args:?} failed");
}

#[tokio::test]
async fn sync_whitelist_then_read_blame_history() {
    let tmp = tempfile::tempdir().unwrap();
    let root = u(tmp.path());

    // ---- build a fixture "remote" repository with two commits ----
    let remote = root.join("remote");
    std::fs::create_dir_all(remote.join("src")).unwrap();
    git(&remote, &["init", "-q", "-b", "main"]);
    std::fs::write(remote.join("src/a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    std::fs::write(remote.join("README.md"), "# hi\n").unwrap();
    std::fs::write(remote.join("blob.bin"), "binary-ish\n").unwrap();
    git(&remote, &["add", "."]);
    git(&remote, &["commit", "-qm", "c1"]);
    // second commit appends a line to a.rs only
    std::fs::write(remote.join("src/a.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
    git(&remote, &["add", "."]);
    git(&remote, &["commit", "-qm", "c2"]);

    // ---- configure the backend against it ----
    let data = root.join("data");
    let url = format!("file:///{}", remote.as_str().replace('\\', "/"));
    let spec = RepoConfig {
        id: RepoId::from("proj"),
        kind: VcsKind::Git,
        url,
        rev: Some("main".into()),
        schedule: None,
        include: vec!["src/**".into(), "*.md".into()],
        exclude: vec!["**/*.bin".into()],
        username: None,
        password: None,
    };
    let ctx = RepoContext::new(&spec, &data);
    let backend = GixBackend::new();

    // ---- sync: only whitelisted files are materialized ----
    let snap = backend.sync(&ctx).await.unwrap();
    assert!(snap.base_path.join("src/a.rs").exists());
    assert!(snap.base_path.join("README.md").exists());
    assert!(!snap.base_path.join("blob.bin").exists(), "excluded file must not materialize");
    assert_eq!(snap.file_count, 2);

    // ---- ranged read ----
    let whole = backend
        .read_file(&ctx, Utf8Path::new("src/a.rs"), &Rev::Head, None)
        .await
        .unwrap();
    assert_eq!(whole.total_lines, 3);
    let first = backend
        .read_file(
            &ctx,
            Utf8Path::new("src/a.rs"),
            &Rev::Head,
            Some(ReadRange::Lines { start: 1, end: Some(1) }),
        )
        .await
        .unwrap();
    assert_eq!(first.text, "fn a() {}");
    assert!(first.truncated);

    // ---- blame ----
    let blame = backend
        .blame(&ctx, Utf8Path::new("src/a.rs"), &Rev::Head)
        .await
        .unwrap();
    assert_eq!(blame.len(), 3);
    assert_eq!(blame[0].author, "Tester");
    assert_eq!(blame[0].line_no, 1);
    // line 3 came from the second commit
    assert_eq!(blame[2].summary, "c2");

    // ---- history: whole repo, newest first ----
    let all = backend.history(&ctx, None, &LogQuery::default()).await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].summary, "c2");
    assert_eq!(all[1].summary, "c1");

    // ---- history: per-file (README only touched in c1) ----
    let readme_hist = backend
        .history(&ctx, Some(Utf8Path::new("README.md")), &LogQuery::default())
        .await
        .unwrap();
    assert_eq!(readme_hist.len(), 1);
    assert_eq!(readme_hist[0].summary, "c1");

    // ---- list_files at head ----
    let files = backend.list_files(&ctx, &Rev::Head).await.unwrap();
    assert!(files.contains(&Utf8PathBuf::from("src/a.rs")));
    assert!(files.contains(&Utf8PathBuf::from("README.md")));
    // list_files reflects the whole tree (not the whitelist), so blob.bin is present
    assert!(files.contains(&Utf8PathBuf::from("blob.bin")));
}

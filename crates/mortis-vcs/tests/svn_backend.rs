//! Integration tests for the SVN backend.
//!
//! These build a throwaway Subversion repository with the system `svnadmin`/
//! `svn` (resolved the same way the server resolves it — embedded first, then
//! system) and exercise sync + whitelist export, ranged reads, blame, history,
//! and `svn list`. The whole test is skipped if no `svn` toolchain is present.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};

use mortis_core::config::{RepoConfig, VcsKind};
use mortis_core::vcs::RepoContext;
use mortis_core::{LogQuery, ReadRange, RepoId, Rev, VcsBackend};
use mortis_vcs::svn::{SvnTool, ToolSource};
use mortis_vcs::SvnCliBackend;

fn u(p: &std::path::Path) -> Utf8PathBuf {
    Utf8PathBuf::from_path_buf(p.to_path_buf()).unwrap()
}

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run(program: &str, args: &[&str]) {
    let ok = Command::new(program).args(args).output().expect("spawn").status.success();
    assert!(ok, "{program} {args:?} failed");
}

/// Build a `file://` URL for an svn repo path (svn wants forward slashes and,
/// on Windows, a leading slash before the drive).
fn file_url(repo: &Utf8Path) -> String {
    let p = repo.as_str().replace('\\', "/");
    if cfg!(windows) {
        format!("file:///{p}")
    } else {
        format!("file://{p}")
    }
}

#[tokio::test]
async fn svn_sync_read_blame_history() {
    if !have("svn") || !have("svnadmin") {
        eprintln!("skipping: svn/svnadmin not installed");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let root = u(tmp.path());

    // ---- create a repository and import an initial tree ----
    let repo = root.join("repo");
    run("svnadmin", &["create", repo.as_str()]);
    let url = file_url(&repo);

    // working copy used only to author content
    let wc = root.join("wc");
    run("svn", &["checkout", &url, wc.as_str()]);
    std::fs::create_dir_all(wc.join("src")).unwrap();
    std::fs::write(wc.join("src/a.txt"), "alpha\nbeta\n").unwrap();
    std::fs::write(wc.join("notes.md"), "# notes\n").unwrap();
    std::fs::write(wc.join("skip.bin"), "nope\n").unwrap();
    run("svn", &["add", wc.join("src").as_str(), wc.join("notes.md").as_str(), wc.join("skip.bin").as_str()]);
    run("svn", &["commit", "-m", "r1", wc.as_str()]);
    // second revision: append a line to a.txt
    std::fs::write(wc.join("src/a.txt"), "alpha\nbeta\ngamma\n").unwrap();
    run("svn", &["commit", "-m", "r2", wc.as_str()]);

    // ---- backend over the repo URL ----
    let tool = SvnTool {
        program: Utf8PathBuf::from("svn"), // resolved from PATH by the OS
        env: Vec::new(),
        source: ToolSource::System,
    };
    let backend = SvnCliBackend::new(tool);

    let data = root.join("data");
    let spec = RepoConfig {
        id: RepoId::from("svnproj"),
        kind: VcsKind::Svn,
        url,
        rev: None,
        schedule: None,
        include: vec!["src/**".into(), "*.md".into()],
        exclude: vec!["**/*.bin".into()],
        username: None,
        password: None,
    };
    let ctx = RepoContext::new(&spec, &data);

    // ---- sync: whitelist export ----
    let snap = backend.sync(&ctx).await.unwrap();
    assert_eq!(snap.head, "2");
    assert!(snap.base_path.join("src/a.txt").exists());
    assert!(snap.base_path.join("notes.md").exists());
    assert!(!snap.base_path.join("skip.bin").exists(), "excluded file must not export");

    // ---- ranged read ----
    let fc = backend
        .read_file(&ctx, Utf8Path::new("src/a.txt"), &Rev::Head, Some(ReadRange::Lines { start: 3, end: Some(3) }))
        .await
        .unwrap();
    assert_eq!(fc.text, "gamma");
    assert_eq!(fc.total_lines, 3);

    // ---- blame ----
    let blame = backend.blame(&ctx, Utf8Path::new("src/a.txt"), &Rev::Head).await.unwrap();
    assert_eq!(blame.len(), 3);
    assert_eq!(blame[0].content, "alpha");
    assert_eq!(blame[2].commit, "2"); // gamma introduced in r2

    // ---- history ----
    let hist = backend.history(&ctx, None, &LogQuery::default()).await.unwrap();
    assert_eq!(hist.len(), 2);
    assert_eq!(hist[0].id, "2");
    assert_eq!(hist[0].summary, "r2");

    // per-file history of notes.md (only r1 touched it)
    let notes_hist = backend
        .history(&ctx, Some(Utf8Path::new("notes.md")), &LogQuery::default())
        .await
        .unwrap();
    assert_eq!(notes_hist.len(), 1);
    assert_eq!(notes_hist[0].id, "1");

    // ---- list_files ----
    let files = backend.list_files(&ctx, &Rev::Head).await.unwrap();
    assert!(files.contains(&Utf8PathBuf::from("src/a.txt")));
    assert!(files.contains(&Utf8PathBuf::from("notes.md")));
}

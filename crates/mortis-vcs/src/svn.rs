//! Read-only Subversion backend driving an `svn` command-line client.
//!
//! The `svn` executable is resolved by [`mortis_embed::resolve_svn`] (embedded
//! → system), so this backend is self-contained when binaries are vendored and
//! falls back to a system `svn` otherwise. All read operations work directly
//! against the repository URL (no persistent working copy); `sync` materializes
//! the whitelisted tree with `svn export` into an immutable, per-revision
//! snapshot `<root>/snapshots/<revnum>` (published atomically; an existing
//! snapshot for the revision is reused and the export skipped). A re-sync to a
//! new revision publishes a new snapshot, so a live session's base is stable.

use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use tokio::process::Command;

use mortis_core::vcs::RepoContext;
use mortis_core::{
    BlameLine, Commit, CoreError, FileContent, LogQuery, ReadRange, RepoSnapshot, Result, Rev,
    Timestamp, VcsBackend, VcsKind, slice_file_content,
};

use crate::filter::GlobFilter;

pub use mortis_embed::{SvnTool, ToolSource};

/// A read-only SVN backend backed by an `svn` CLI.
#[derive(Debug, Clone)]
pub struct SvnCliBackend {
    tool: SvnTool,
}

impl SvnCliBackend {
    pub fn new(tool: SvnTool) -> Self {
        Self { tool }
    }

    /// Resolve an `svn` tool (embedded → system, or an explicit override) and
    /// build a backend. `cache_dir` is where embedded binaries are extracted.
    pub fn resolve(cache_dir: &Utf8Path, override_path: Option<&Utf8Path>) -> Result<Self> {
        Ok(Self::new(mortis_embed::resolve_svn(cache_dir, override_path)?))
    }

    /// Where the resolved svn came from (embedded/system/override).
    pub fn source(&self) -> ToolSource {
        self.tool.source
    }

    /// Run `svn` with the given args (always non-interactive), returning stdout.
    async fn run(&self, spec_user: Option<&str>, spec_pass: Option<&str>, args: &[&str]) -> Result<Vec<u8>> {
        let mut cmd = Command::new(self.tool.program.as_std_path());
        cmd.arg("--non-interactive");
        if let Some(u) = spec_user {
            cmd.args(["--username", u]);
        }
        if let Some(p) = spec_pass {
            cmd.args(["--password", p]);
        }
        cmd.args(args);
        for (k, v) in &self.tool.env {
            cmd.env(k, v);
        }
        let output = cmd
            .output()
            .await
            .map_err(|e| CoreError::Vcs(format!("failed to spawn svn: {e}")))?;
        if !output.status.success() {
            return Err(CoreError::Vcs(format!(
                "svn {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(output.stdout)
    }
}

/// Build a peg-revision target URL: `<url>[/<path>]@<rev>`.
fn target(url: &str, path: Option<&Utf8Path>, rev: &Rev) -> String {
    let mut t = url.trim_end_matches('/').to_string();
    if let Some(p) = path {
        t.push('/');
        t.push_str(p.as_str());
    }
    match rev {
        Rev::Head => t.push_str("@HEAD"),
        Rev::At(r) => {
            t.push('@');
            t.push_str(r);
        }
    }
    t
}

fn rev_of(ctx: &RepoContext<'_>, at: &Rev) -> Rev {
    // For the configured default, honor spec.rev; otherwise the explicit `at`.
    match at {
        Rev::Head => Rev::from_opt(ctx.spec.rev.clone()),
        other => other.clone(),
    }
}

/// Parse an svn RFC3339 timestamp (e.g. `2024-06-24T12:34:56.123456Z`).
fn parse_time(date: Option<&str>) -> Timestamp {
    date.and_then(|d| humantime::parse_rfc3339(d.trim()).ok())
        .map(Timestamp::from_system)
        .unwrap_or(Timestamp(0))
}

#[async_trait]
impl VcsBackend for SvnCliBackend {
    fn kind(&self) -> VcsKind {
        VcsKind::Svn
    }

    async fn sync(&self, ctx: &RepoContext<'_>) -> Result<RepoSnapshot> {
        let rev = rev_of(ctx, &Rev::Head);
        let url = &ctx.spec.url;
        let user = ctx.spec.username.as_deref();
        let pass = ctx.spec.password.as_deref();

        // Resolve the concrete revision number for the snapshot.
        let info = self
            .run(user, pass, &["info", "--show-item", "revision", &target(url, None, &rev)])
            .await?;
        let head = String::from_utf8_lossy(&info).trim().to_string();

        // Export the tree (no .svn metadata) to a temp dir, then materialize the
        // whitelisted subset into an immutable, per-revision snapshot. Skip the
        // export entirely when this revision is already materialized.
        let export_dir = ctx.internal_dir().join("export");
        if !ctx.snapshot_dir(&head).exists() {
            if export_dir.exists() {
                std::fs::remove_dir_all(&export_dir)?;
            }
            if let Some(parent) = export_dir.parent() {
                std::fs::create_dir_all(parent)?;
            }
            self.run(
                user,
                pass,
                &["export", "--force", &target(url, None, &rev), export_dir.as_str()],
            )
            .await?;
        }

        let filter = GlobFilter::new(&ctx.spec.include, &ctx.spec.exclude)?;
        let (base_path, count) =
            crate::publish::publish_snapshot(&ctx.snapshots_dir(), &head, |staging| {
                let mut count = 0usize;
                copy_filtered(&export_dir, &export_dir, staging, &filter, &mut count)?;
                Ok(count)
            })?;
        // The export is just a staging area; reclaim its space.
        std::fs::remove_dir_all(&export_dir).ok();

        Ok(RepoSnapshot {
            repo: ctx.spec.id.clone(),
            head,
            base_path,
            synced_at: Timestamp::now(),
            file_count: count,
        })
    }

    async fn list_files(&self, ctx: &RepoContext<'_>, at: &Rev) -> Result<Vec<Utf8PathBuf>> {
        let rev = rev_of(ctx, at);
        let out = self
            .run(
                ctx.spec.username.as_deref(),
                ctx.spec.password.as_deref(),
                &["list", "-R", &target(&ctx.spec.url, None, &rev)],
            )
            .await?;
        let mut files: Vec<Utf8PathBuf> = String::from_utf8_lossy(&out)
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.is_empty() && !l.ends_with('/')) // drop directory entries
            .map(Utf8PathBuf::from)
            .collect();
        files.sort();
        Ok(files)
    }

    async fn read_file(
        &self,
        ctx: &RepoContext<'_>,
        path: &Utf8Path,
        at: &Rev,
        range: Option<ReadRange>,
    ) -> Result<FileContent> {
        let rev = rev_of(ctx, at);
        let bytes = self
            .run(
                ctx.spec.username.as_deref(),
                ctx.spec.password.as_deref(),
                &["cat", &target(&ctx.spec.url, Some(path), &rev)],
            )
            .await?;
        Ok(slice_file_content(path.to_owned(), &bytes, range))
    }

    async fn blame(&self, ctx: &RepoContext<'_>, path: &Utf8Path, at: &Rev) -> Result<Vec<BlameLine>> {
        let rev = rev_of(ctx, at);
        let user = ctx.spec.username.as_deref();
        let pass = ctx.spec.password.as_deref();
        let tgt = target(&ctx.spec.url, Some(path), &rev);

        let xml = self.run(user, pass, &["blame", "--xml", &tgt]).await?;
        let parsed: BlameXml = quick_xml::de::from_str(&String::from_utf8_lossy(&xml))
            .map_err(|e| CoreError::Vcs(format!("parse svn blame xml: {e}")))?;

        // Blame XML lacks line content; fetch it separately and zip by line.
        let content = self.run(user, pass, &["cat", &tgt]).await?;
        let content = String::from_utf8_lossy(&content);
        let lines: Vec<&str> = content.lines().collect();

        let mut out = Vec::new();
        for target_block in parsed.target {
            for entry in target_block.entry {
                let text = lines
                    .get(entry.line_number.saturating_sub(1) as usize)
                    .copied()
                    .unwrap_or("")
                    .to_string();
                let (commit, author, time) = match entry.commit {
                    Some(c) => (c.revision, c.author.unwrap_or_default(), parse_time(c.date.as_deref())),
                    None => (String::new(), String::new(), Timestamp(0)),
                };
                out.push(BlameLine {
                    line_no: entry.line_number,
                    commit,
                    author,
                    author_email: String::new(), // svn has no separate email
                    time,
                    summary: String::new(),
                    content: text,
                });
            }
        }
        Ok(out)
    }

    async fn history(
        &self,
        ctx: &RepoContext<'_>,
        path: Option<&Utf8Path>,
        query: &LogQuery,
    ) -> Result<Vec<Commit>> {
        let rev = rev_of(ctx, &Rev::Head);
        let skip = query.skip.unwrap_or(0);
        let want = query.max_count.unwrap_or(usize::MAX);
        // Fetch enough to cover skip+want when both are bounded.
        let fetch = skip.saturating_add(want);

        let tgt = target(&ctx.spec.url, path, &rev);
        let mut args: Vec<String> = vec!["log".into(), "--xml".into()];
        if fetch != usize::MAX {
            args.push("-l".into());
            args.push(fetch.to_string());
        }
        args.push(tgt);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

        let xml = self
            .run(ctx.spec.username.as_deref(), ctx.spec.password.as_deref(), &arg_refs)
            .await?;
        let parsed: LogXml = quick_xml::de::from_str(&String::from_utf8_lossy(&xml))
            .map_err(|e| CoreError::Vcs(format!("parse svn log xml: {e}")))?;

        let commits = parsed
            .logentry
            .into_iter()
            .skip(skip)
            .take(want)
            .map(|e| {
                let message = e.msg.unwrap_or_default();
                let summary = message.lines().next().unwrap_or("").to_string();
                Commit {
                    id: e.revision,
                    author: e.author.unwrap_or_default(),
                    author_email: String::new(),
                    time: parse_time(e.date.as_deref()),
                    summary,
                    message,
                    parents: Vec::new(), // svn history is linear
                }
            })
            .collect();
        Ok(commits)
    }
}

/// Recursively copy whitelisted files from `dir` (under `root`) into `work`.
fn copy_filtered(
    root: &Utf8Path,
    dir: &Utf8Path,
    work: &Utf8Path,
    filter: &GlobFilter,
    count: &mut usize,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|p| CoreError::Vcs(format!("non-utf8 path: {}", p.display())))?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_filtered(root, &path, work, filter, count)?;
        } else if ft.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                let logical = Utf8PathBuf::from(rel.as_str().replace('\\', "/"));
                if filter.matches(&logical) {
                    let dest = work.join(&logical);
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::copy(&path, &dest)?;
                    *count += 1;
                }
            }
        }
    }
    Ok(())
}

// --------------------------- svn --xml deserialization structs ---------------

#[derive(Debug, Deserialize)]
struct BlameXml {
    #[serde(default)]
    target: Vec<BlameTarget>,
}

#[derive(Debug, Deserialize)]
struct BlameTarget {
    #[serde(default)]
    entry: Vec<BlameEntryXml>,
}

#[derive(Debug, Deserialize)]
struct BlameEntryXml {
    #[serde(rename = "@line-number")]
    line_number: u32,
    commit: Option<BlameCommitXml>,
}

#[derive(Debug, Deserialize)]
struct BlameCommitXml {
    #[serde(rename = "@revision")]
    revision: String,
    author: Option<String>,
    date: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LogXml {
    #[serde(default)]
    logentry: Vec<LogEntryXml>,
}

#[derive(Debug, Deserialize)]
struct LogEntryXml {
    #[serde(rename = "@revision")]
    revision: String,
    author: Option<String>,
    date: Option<String>,
    msg: Option<String>,
}

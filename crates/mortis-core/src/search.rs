//! The [`SearchEngine`] trait and its query/result types.
//!
//! Search is intentionally synchronous and blocking — it is CPU-bound and built
//! on the ripgrep crates. The application layer runs it on a blocking task pool.
//! It operates over any [`FileView`], so the same engine serves both bare repos
//! and session overlays.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::model::RepoId;
use crate::view::FileView;

/// Case-sensitivity policy for a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaseMode {
    /// Case-insensitive unless the pattern contains an uppercase letter.
    #[default]
    Smart,
    Sensitive,
    Insensitive,
}

/// A code-search request scoped within a single [`FileView`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchQuery {
    /// The pattern. Interpreted as a regex when `regex` is true, else literal.
    pub pattern: String,
    #[serde(default)]
    pub regex: bool,
    #[serde(default)]
    pub case: CaseMode,
    /// Stop after this many matches across all files. `None` = unbounded.
    #[serde(default)]
    pub max_results: Option<usize>,
    /// Number of context lines to include before each match.
    #[serde(default)]
    pub context_before: usize,
    /// Number of context lines to include after each match.
    #[serde(default)]
    pub context_after: usize,
    /// Restrict the walk to this subtree (relative to the view root).
    #[serde(default)]
    pub subtree: Option<Utf8PathBuf>,
    /// Restrict to files matching these globs (e.g. `["*.rs", "src/**"]`).
    #[serde(default)]
    pub globs: Vec<String>,
}

impl SearchQuery {
    /// A minimal literal query for `pattern`.
    pub fn literal(pattern: impl Into<String>) -> Self {
        SearchQuery {
            pattern: pattern.into(),
            regex: false,
            case: CaseMode::default(),
            max_results: None,
            context_before: 0,
            context_after: 0,
            subtree: None,
            globs: Vec::new(),
        }
    }
}

/// A single match (one line) produced by a search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMatch {
    /// Repo this match came from (set by the service when searching repos).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<RepoId>,
    /// Logical path of the file, relative to the view root.
    pub path: Utf8PathBuf,
    /// 1-based line number of the matching line.
    pub line_no: u64,
    /// The full matching line (trailing newline stripped).
    pub line: String,
    /// `(start, end)` byte offsets of the matched spans within `line`.
    #[serde(default)]
    pub submatches: Vec<(u32, u32)>,
    /// Context lines preceding the match.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub before: Vec<String>,
    /// Context lines following the match.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
}

/// Control-flow signal returned by a streaming search sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Keep producing matches.
    Continue,
    /// Stop the search early.
    Stop,
}

/// Cooperative cancellation flag for an in-progress search.
///
/// Deliberately a std `Arc<AtomicBool>` rather than `tokio_util::CancellationToken`
/// so this crate (and `mortis-search`) stay free of any async runtime. The
/// application flips it from a drop guard; the engine polls [`is_cancelled`] at
/// file boundaries to stop a detached blocking walk promptly.
///
/// [`is_cancelled`]: CancelToken::is_cancelled
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A fresh, not-yet-cancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. Idempotent.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether cancellation has been requested. `Relaxed` suffices — this is a
    /// best-effort early-exit hint, not a fence guarding other memory.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// An embedded code-search engine.
pub trait SearchEngine: Send + Sync {
    /// Stream matches to `sink`, which may request early termination.
    ///
    /// `cancel` is polled at file boundaries; a cancelled token stops the walk
    /// promptly (used to abandon a detached blocking search whose future was
    /// dropped). This is the primitive; [`SearchEngine::search`] is the buffered
    /// helper.
    fn search_streaming(
        &self,
        view: &dyn FileView,
        query: &SearchQuery,
        cancel: &CancelToken,
        sink: &mut dyn FnMut(SearchMatch) -> Flow,
    ) -> Result<()>;

    /// Collect all matches (respecting `max_results`) into a vector.
    fn search(&self, view: &dyn FileView, query: &SearchQuery) -> Result<Vec<SearchMatch>> {
        self.search_cancellable(view, query, &CancelToken::new())
    }

    /// Like [`search`](SearchEngine::search) but cancellable via `cancel`.
    fn search_cancellable(
        &self,
        view: &dyn FileView,
        query: &SearchQuery,
        cancel: &CancelToken,
    ) -> Result<Vec<SearchMatch>> {
        let max = query.max_results;
        let mut out = Vec::new();
        self.search_streaming(view, query, cancel, &mut |m| {
            out.push(m);
            match max {
                Some(limit) if out.len() >= limit => Flow::Stop,
                _ => Flow::Continue,
            }
        })?;
        Ok(out)
    }
}

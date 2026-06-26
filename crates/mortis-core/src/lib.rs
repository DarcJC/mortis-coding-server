//! # mortis-core
//!
//! The domain contract for `mortis-code-server`. This crate is deliberately
//! framework-free: it defines the *vocabulary* (value types), the *ports*
//! (traits implemented by infrastructure crates), and the shared error type.
//! Everything else in the workspace depends on this crate and nothing here
//! depends on `axum`, `gix`, `rmcp`, etc.
//!
//! ## Ports (traits)
//! - [`vcs::VcsBackend`] — read-only Git/SVN access (sync, read, blame, log).
//! - [`search::SearchEngine`] — embedded ripgrep over a [`view::FileView`].
//! - [`session::SessionStore`] — copy-on-write write layer with diff/patch.
//! - [`view::FileView`] — the seam that lets search/read work on either a bare
//!   repo or a session overlay.

pub mod asm;
pub mod config;
pub mod error;
pub mod model;
pub mod search;
pub mod session;
pub mod vcs;
pub mod view;

pub use error::{CoreError, Result};

// Re-export the most-used types at the crate root for ergonomic imports.
pub use asm::{
    AsmDownloadPolicy, AsmSession, AsmSessionId, AsmStatus, AssemblyStore, BinaryFormat, BinaryInfo,
    BinaryOs, Disassembly, FunctionResolution, Instruction, SectionInfo, SegmentInfo,
};
pub use config::{RepoConfig, VcsKind};
pub use model::{
    FileContent, Principal, ReadRange, RepoId, Rev, SessionId, Timestamp, ensure_safe_relative,
    line_range, slice_file_content,
};
pub use search::{CancelToken, CaseMode, Flow, SearchEngine, SearchMatch, SearchQuery};
pub use session::{
    ChangeKind, EditOutcome, FileEdit, FileStatus, Replacement, Session, SessionStore,
};
pub use vcs::{BlameLine, Commit, LogQuery, RepoContext, RepoSnapshot, VcsBackend};
pub use view::FileView;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rev_from_opt_normalizes_head() {
        assert_eq!(Rev::from_opt(None), Rev::Head);
        assert_eq!(Rev::from_opt(Some(String::new())), Rev::Head);
        assert_eq!(Rev::from_opt(Some("HEAD".into())), Rev::Head);
        assert_eq!(Rev::from_opt(Some("main".into())), Rev::At("main".into()));
    }

    #[test]
    fn timestamp_roundtrips_through_system_time() {
        let t = Timestamp(1_700_000_000_000);
        assert_eq!(Timestamp::from_system(t.to_system()), t);
    }

    #[test]
    fn slice_whole_file_counts_lines() {
        let fc = slice_file_content("a.txt".into(), b"l1\nl2\nl3\n", None);
        assert_eq!(fc.total_lines, 3);
        assert_eq!((fc.start_line, fc.end_line), (1, 3));
        assert!(!fc.truncated);
        assert_eq!(fc.text, "l1\nl2\nl3\n");

        let no_trailing = slice_file_content("a.txt".into(), b"l1\nl2", None);
        assert_eq!(no_trailing.total_lines, 2);
    }

    #[test]
    fn slice_line_range_is_inclusive_and_marks_truncation() {
        let fc = slice_file_content(
            "a.txt".into(),
            b"l1\nl2\nl3\nl4\nl5\n",
            Some(ReadRange::Lines { start: 2, end: Some(4) }),
        );
        assert_eq!(fc.text, "l2\nl3\nl4");
        assert_eq!((fc.start_line, fc.end_line, fc.total_lines), (2, 4, 5));
        assert!(fc.truncated);
    }

    #[test]
    fn slice_detects_binary() {
        let fc = slice_file_content("b.bin".into(), b"\x00\x01\x02", None);
        assert!(fc.is_binary);
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(CoreError::not_found("x").code(), "not_found");
        assert_eq!(CoreError::invalid("y").code(), "invalid_input");
    }

    #[test]
    fn repo_config_whitelist_detection() {
        let json = serde_json::json!({
            "id": "r1",
            "kind": "git",
            "url": "https://example.com/r.git",
            "include": ["src/**"]
        });
        let cfg: RepoConfig = serde_json::from_value(json).unwrap();
        assert!(cfg.has_whitelist());
        assert_eq!(cfg.kind, VcsKind::Git);
    }

    #[test]
    fn search_query_respects_max_results_via_default_search() {
        // A tiny fake view + engine to exercise the buffered `search` default.
        use camino::{Utf8Path, Utf8PathBuf};

        struct EmptyView(Utf8PathBuf);
        impl FileView for EmptyView {
            fn root(&self) -> &Utf8Path {
                &self.0
            }
            fn list_files(&self, _s: Option<&Utf8Path>) -> Result<Vec<Utf8PathBuf>> {
                Ok(vec![])
            }
            fn resolve(&self, _l: &Utf8Path) -> Result<Option<Utf8PathBuf>> {
                Ok(None)
            }
            fn read(&self, _l: &Utf8Path) -> Result<Vec<u8>> {
                Err(CoreError::not_found("x"))
            }
        }

        struct FloodEngine;
        impl SearchEngine for FloodEngine {
            fn search_streaming(
                &self,
                _v: &dyn FileView,
                _q: &SearchQuery,
                _cancel: &CancelToken,
                sink: &mut dyn FnMut(SearchMatch) -> Flow,
            ) -> Result<()> {
                for i in 0..1000u64 {
                    let m = SearchMatch {
                        repo: None,
                        path: "f".into(),
                        line_no: i,
                        line: "x".into(),
                        submatches: vec![],
                        before: vec![],
                        after: vec![],
                    };
                    if sink(m) == Flow::Stop {
                        break;
                    }
                }
                Ok(())
            }
        }

        let view = EmptyView(Utf8PathBuf::from("/"));
        let mut q = SearchQuery::literal("x");
        q.max_results = Some(10);
        let hits = FloodEngine.search(&view, &q).unwrap();
        assert_eq!(hits.len(), 10);
    }
}

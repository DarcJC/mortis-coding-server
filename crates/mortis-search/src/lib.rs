//! # mortis-search
//!
//! An embedded code-search engine built directly on the ripgrep crates
//! ([`grep_regex`], [`grep_searcher`], [`grep_matcher`]). It implements the
//! [`SearchEngine`](mortis_core::SearchEngine) trait over any
//! [`FileView`](mortis_core::FileView), so the very same engine searches a bare
//! repository working tree and a copy-on-write session overlay without caring
//! which it is.
//!
//! There is no dependency on an external `rg` binary: candidate files are
//! enumerated through the view, read into memory once, and searched in-process
//! with [`Searcher::search_slice`]. Binary files are detected and skipped.
//!
//! Search is synchronous and CPU-bound by design; the application layer is
//! expected to run it on a blocking task pool.

use camino::{Utf8Path, Utf8PathBuf};
use globset::{Glob, GlobSet, GlobSetBuilder};
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};

use mortis_core::{CaseMode, CoreError, FileView, Flow, Result, SearchEngine, SearchMatch, SearchQuery};

/// An embedded ripgrep-backed [`SearchEngine`].
///
/// Stateless and zero-sized: construct one with `GrepSearchEngine` and share it
/// freely across threads.
#[derive(Debug, Clone, Copy, Default)]
pub struct GrepSearchEngine;

impl GrepSearchEngine {
    /// Construct the engine. (Provided for symmetry; the unit value works too.)
    pub fn new() -> Self {
        GrepSearchEngine
    }

    /// Build a [`RegexMatcher`] from the query's pattern and flags.
    ///
    /// Literal queries lean on grep-regex's own `fixed_strings` knob rather than
    /// escaping metacharacters by hand — it produces the same result and keeps
    /// the literal/regex paths symmetric.
    fn build_matcher(query: &SearchQuery) -> Result<RegexMatcher> {
        let mut builder = RegexMatcherBuilder::new();
        match query.case {
            CaseMode::Smart => {
                builder.case_smart(true);
            }
            CaseMode::Insensitive => {
                builder.case_insensitive(true);
            }
            CaseMode::Sensitive => {
                // Defaults are case-sensitive; nothing to toggle.
            }
        }
        if !query.regex {
            // Treat the pattern as a literal string.
            builder.fixed_strings(true);
        }
        builder
            .build(&query.pattern)
            .map_err(|e| CoreError::Search(format!("invalid pattern {:?}: {e}", query.pattern)))
    }

    /// Compile the query's globs into a matcher, or `None` when no globs are set.
    fn build_globset(query: &SearchQuery) -> Result<Option<GlobSet>> {
        if query.globs.is_empty() {
            return Ok(None);
        }
        let mut builder = GlobSetBuilder::new();
        for pat in &query.globs {
            let glob = Glob::new(pat)
                .map_err(|e| CoreError::Search(format!("invalid glob {pat:?}: {e}")))?;
            builder.add(glob);
        }
        let set = builder
            .build()
            .map_err(|e| CoreError::Search(format!("invalid glob set: {e}")))?;
        Ok(Some(set))
    }
}

impl SearchEngine for GrepSearchEngine {
    fn search_streaming(
        &self,
        view: &dyn FileView,
        query: &SearchQuery,
        sink: &mut dyn FnMut(SearchMatch) -> Flow,
    ) -> Result<()> {
        let matcher = Self::build_matcher(query)?;
        let globs = Self::build_globset(query)?;

        // A fresh searcher is cheap; reuse one across all candidate files.
        // Context is computed from the file's own line array (see MatchSink),
        // so the searcher itself does not need before/after context.
        let mut searcher: Searcher = SearcherBuilder::new()
            .line_number(true)
            .binary_detection(BinaryDetection::quit(0))
            .build();

        let subtree = query.subtree.as_deref();
        let candidates = view.list_files(subtree)?;

        for logical in candidates {
            // Honor `globs`: keep only logical paths matching at least one glob.
            if let Some(set) = &globs {
                if !set.is_match(logical.as_std_path()) {
                    continue;
                }
            }

            // Map to the on-disk file; a `None` means the path vanished from the
            // view (e.g. a whiteout) between listing and resolving — skip it.
            let Some(_disk) = view.resolve(&logical)? else {
                continue;
            };

            // Read the bytes once. Going through the view keeps overlay
            // semantics intact (upper shadows base) rather than touching disk
            // directly.
            let bytes = view.read(&logical)?;

            let mut sink_impl = MatchSink::new(&matcher, &logical, &bytes, query, sink);
            // `search_slice` reports failures through the sink's error type,
            // which is already `CoreError`. Tag it with the file for context.
            searcher
                .search_slice(&matcher, &bytes, &mut sink_impl)
                .map_err(|e| CoreError::Search(format!("search failed in {logical}: {e}")))?;

            // Surface an error the sink stashed while recovering submatches.
            if let Some(err) = sink_impl.error.take() {
                return Err(err);
            }
            if sink_impl.stopped {
                // The caller asked us to stop (e.g. `max_results` reached);
                // abandon the remaining files.
                break;
            }
        }
        Ok(())
    }
}

/// Split `bytes` into logical lines (terminators stripped), lossily decoded.
///
/// Used to materialize before/after context windows around a match. The result
/// is 0-indexed; the searcher reports 1-based line numbers, so callers subtract
/// one before indexing.
fn split_lines_lossy(bytes: &[u8]) -> Vec<String> {
    // `split` on `\n` then trim a trailing `\r` matches how the searcher counts
    // lines for both LF and CRLF inputs. A trailing newline yields a final empty
    // element which is harmless (it is simply never indexed by a match).
    bytes
        .split(|&b| b == b'\n')
        .map(|line| {
            let line = strip_cr(line);
            String::from_utf8_lossy(line).into_owned()
        })
        .collect()
}

/// Strip a single trailing `\r` (for CRLF line endings).
fn strip_cr(line: &[u8]) -> &[u8] {
    match line.last() {
        Some(b'\r') => &line[..line.len() - 1],
        _ => line,
    }
}

/// Strip a trailing `\r?\n` from a raw matched-line slice.
fn strip_line_terminator(line: &[u8]) -> &[u8] {
    let line = match line.last() {
        Some(b'\n') => &line[..line.len() - 1],
        _ => line,
    };
    strip_cr(line)
}

/// A [`Sink`] that turns each matched line into a [`SearchMatch`] and forwards
/// it to the user-supplied closure.
///
/// It borrows the file's raw bytes (to compute context windows lazily) and the
/// compiled matcher (to recover submatch spans within each line).
struct MatchSink<'a> {
    matcher: &'a RegexMatcher,
    /// Logical path of the file being searched (what callers see in results).
    path: &'a Utf8Path,
    /// The file split into context-able lines, built on first use.
    lines: Option<Vec<String>>,
    /// Raw bytes, kept so `lines` can be materialized lazily.
    raw: &'a [u8],
    query: &'a SearchQuery,
    /// The user's streaming sink.
    sink: &'a mut dyn FnMut(SearchMatch) -> Flow,
    /// Set once the user's sink returns [`Flow::Stop`].
    stopped: bool,
    /// First error raised while invoking the matcher inside the sink, if any.
    error: Option<CoreError>,
}

impl<'a> MatchSink<'a> {
    fn new(
        matcher: &'a RegexMatcher,
        path: &'a Utf8Path,
        raw: &'a [u8],
        query: &'a SearchQuery,
        sink: &'a mut dyn FnMut(SearchMatch) -> Flow,
    ) -> Self {
        MatchSink {
            matcher,
            path,
            lines: None,
            raw,
            query,
            sink,
            stopped: false,
            error: None,
        }
    }

    /// Lazily split the file into lines for context extraction.
    fn lines(&mut self) -> &[String] {
        if self.lines.is_none() {
            self.lines = Some(split_lines_lossy(self.raw));
        }
        self.lines.as_deref().unwrap()
    }

    /// Collect `count` lines ending just before 0-based `idx` (context-before).
    fn before(&mut self, idx: usize, count: usize) -> Vec<String> {
        if count == 0 || idx == 0 {
            return Vec::new();
        }
        let lines = self.lines();
        let start = idx.saturating_sub(count);
        lines[start..idx].to_vec()
    }

    /// Collect `count` lines following 0-based `idx` (context-after).
    fn after(&mut self, idx: usize, count: usize) -> Vec<String> {
        if count == 0 {
            return Vec::new();
        }
        let lines = self.lines();
        let start = idx + 1;
        if start >= lines.len() {
            return Vec::new();
        }
        let end = (start + count).min(lines.len());
        lines[start..end].to_vec()
    }
}

impl Sink for MatchSink<'_> {
    // The searcher requires `Sink::Error: SinkError`, which `CoreError` cannot
    // implement here (orphan rule). We use `io::Error` as the wire type and
    // carry any real `CoreError` out-of-band via `self.error` instead.
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        mat: &SinkMatch<'_>,
    ) -> std::result::Result<bool, Self::Error> {
        // With multi-line disabled the matcher reports exactly one line here.
        let line_no = mat.line_number().unwrap_or(0);
        let raw_line = strip_line_terminator(mat.bytes());

        // Recover submatch spans relative to the start of this line. The regex
        // matcher's error type is `NoError`, so this never actually fails; we
        // still thread any error out defensively.
        let mut submatches: Vec<(u32, u32)> = Vec::new();
        let find = self.matcher.find_iter(raw_line, |m| {
            submatches.push((m.start() as u32, m.end() as u32));
            true
        });
        if let Err(e) = find {
            self.error = Some(CoreError::Search(format!("submatch scan failed: {e}")));
            return Ok(false);
        }

        let line = String::from_utf8_lossy(raw_line).into_owned();

        // Context windows, computed from the file's line array. `line_no` is
        // 1-based; convert to a 0-based index into `lines`.
        let (before, after) = if line_no >= 1 {
            let idx = (line_no - 1) as usize;
            let before = self.before(idx, self.query.context_before);
            let after = self.after(idx, self.query.context_after);
            (before, after)
        } else {
            (Vec::new(), Vec::new())
        };

        let search_match = SearchMatch {
            // The service fills in the repo id later; the engine is repo-agnostic.
            repo: None,
            path: Utf8PathBuf::from(self.path),
            line_no,
            line,
            submatches,
            before,
            after,
        };

        match (self.sink)(search_match) {
            Flow::Continue => Ok(true),
            Flow::Stop => {
                self.stopped = true;
                Ok(false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use mortis_fs::PhysicalFileView;
    use std::fs;

    /// Build a small fixture tree and return a view over it (plus the tempdir,
    /// which must be kept alive for the duration of the test).
    fn fixture() -> (tempfile::TempDir, PhysicalFileView) {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();

        fs::write(
            root.join("src/a.rs"),
            "fn alpha() {}\nlet TODO = 1;\nfn beta() {}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/b.rs"),
            "struct Beta;\n// todo: nothing\nfn gamma() {}\n",
        )
        .unwrap();
        fs::write(
            root.join("README.md"),
            "# Title\nThis project has a TODO item.\nDone.\n",
        )
        .unwrap();

        let view = PhysicalFileView::new(root);
        (tmp, view)
    }

    /// Sort matches by (path, line) for order-independent assertions.
    fn sorted(mut v: Vec<SearchMatch>) -> Vec<SearchMatch> {
        v.sort_by_key(|m| (m.path.clone(), m.line_no));
        v
    }

    #[test]
    fn literal_match_is_not_regex_interpreted() {
        let (_t, view) = fixture();
        let engine = GrepSearchEngine::new();

        // A literal query for "()" must match the parentheses in fn signatures,
        // not be treated as an (empty) regex group.
        let q = SearchQuery::literal("()");
        let hits = sorted(engine.search(&view, &q).unwrap());
        // Three `fn ...()` lines across a.rs (2) and b.rs (1).
        assert_eq!(hits.len(), 3);
        assert!(hits.iter().all(|h| h.line.contains("()")));
    }

    #[test]
    fn literal_special_chars_are_escaped() {
        let (_t, view) = fixture();
        let engine = GrepSearchEngine::new();

        // "." as a literal should only match the line containing a real dot.
        let mut q = SearchQuery::literal(".");
        q.case = CaseMode::Sensitive;
        let hits = sorted(engine.search(&view, &q).unwrap());
        // Lines with an actual '.': README "TODO item." and "Done.", b.rs "// todo: nothing" has none...
        // Only the two README lines contain '.'.
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.path == "README.md"));
    }

    #[test]
    fn regex_match_uses_metacharacters() {
        let (_t, view) = fixture();
        let engine = GrepSearchEngine::new();

        // Regex: a `fn <name>(` definition.
        let mut q = SearchQuery::literal(r"fn \w+\(");
        q.regex = true;
        let hits = sorted(engine.search(&view, &q).unwrap());
        assert_eq!(hits.len(), 3);
        // alpha, beta, gamma
        assert!(hits.iter().any(|h| h.line.contains("alpha")));
        assert!(hits.iter().any(|h| h.line.contains("gamma")));
    }

    #[test]
    fn case_smart_is_insensitive_for_lowercase_pattern() {
        let (_t, view) = fixture();
        let engine = GrepSearchEngine::new();

        // Lowercase pattern under Smart => case-insensitive: matches TODO and todo.
        let mut q = SearchQuery::literal("todo");
        q.case = CaseMode::Smart;
        let hits = engine.search(&view, &q).unwrap();
        // a.rs "TODO", b.rs "todo", README "TODO" => 3
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn case_smart_is_sensitive_for_uppercase_pattern() {
        let (_t, view) = fixture();
        let engine = GrepSearchEngine::new();

        // Uppercase letter present under Smart => case-sensitive: only "TODO".
        let mut q = SearchQuery::literal("TODO");
        q.case = CaseMode::Smart;
        let hits = sorted(engine.search(&view, &q).unwrap());
        assert_eq!(hits.len(), 2); // a.rs and README, NOT b.rs "todo"
        assert!(hits.iter().all(|h| h.line.contains("TODO")));
    }

    #[test]
    fn case_sensitive_matches_exact_case_only() {
        let (_t, view) = fixture();
        let engine = GrepSearchEngine::new();

        let mut q = SearchQuery::literal("todo");
        q.case = CaseMode::Sensitive;
        let hits = engine.search(&view, &q).unwrap();
        assert_eq!(hits.len(), 1); // only b.rs "todo"
        assert_eq!(hits[0].path, "src/b.rs");
    }

    #[test]
    fn case_insensitive_matches_all_cases() {
        let (_t, view) = fixture();
        let engine = GrepSearchEngine::new();

        let mut q = SearchQuery::literal("BETA");
        q.case = CaseMode::Insensitive;
        let hits = sorted(engine.search(&view, &q).unwrap());
        // a.rs "fn beta()", b.rs "struct Beta;" => 2
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn subtree_scopes_the_walk() {
        let (_t, view) = fixture();
        let engine = GrepSearchEngine::new();

        let mut q = SearchQuery::literal("TODO");
        q.case = CaseMode::Insensitive;
        q.subtree = Some(Utf8PathBuf::from("src"));
        let hits = sorted(engine.search(&view, &q).unwrap());
        // Only a.rs and b.rs are under src/; README is excluded.
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.path.starts_with("src")));
    }

    #[test]
    fn globs_filter_candidate_files() {
        let (_t, view) = fixture();
        let engine = GrepSearchEngine::new();

        let mut q = SearchQuery::literal("TODO");
        q.case = CaseMode::Insensitive;
        q.globs = vec!["*.rs".to_string()];
        let hits = sorted(engine.search(&view, &q).unwrap());
        // README.md excluded by glob; only the two .rs files remain.
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.path.as_str().ends_with(".rs")));
    }

    #[test]
    fn globs_can_match_nested_paths() {
        let (_t, view) = fixture();
        let engine = GrepSearchEngine::new();

        // `src/**` should match files under src/ at any depth.
        let mut q = SearchQuery::literal("fn");
        q.case = CaseMode::Insensitive;
        q.globs = vec!["src/**".to_string()];
        let hits = engine.search(&view, &q).unwrap();
        assert!(hits.iter().all(|h| h.path.starts_with("src")));
        assert!(!hits.is_empty());
    }

    #[test]
    fn context_before_and_after_return_neighbor_lines() {
        let (_t, view) = fixture();
        let engine = GrepSearchEngine::new();

        // Match the middle line of a.rs ("let TODO = 1;") and ask for 1 line
        // of context on each side.
        let mut q = SearchQuery::literal("let TODO");
        q.case = CaseMode::Sensitive;
        q.context_before = 1;
        q.context_after = 1;
        q.globs = vec!["src/a.rs".to_string()];
        let hits = engine.search(&view, &q).unwrap();
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert_eq!(h.line_no, 2);
        assert_eq!(h.before, vec!["fn alpha() {}".to_string()]);
        assert_eq!(h.after, vec!["fn beta() {}".to_string()]);
    }

    #[test]
    fn context_clamps_at_file_boundaries() {
        let (_t, view) = fixture();
        let engine = GrepSearchEngine::new();

        // Match the FIRST line; before-context must be empty, not panic.
        let mut q = SearchQuery::literal("fn alpha");
        q.case = CaseMode::Sensitive;
        q.context_before = 3;
        q.context_after = 1;
        q.globs = vec!["src/a.rs".to_string()];
        let hits = engine.search(&view, &q).unwrap();
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert_eq!(h.line_no, 1);
        assert!(h.before.is_empty());
        assert_eq!(h.after, vec!["let TODO = 1;".to_string()]);
    }

    #[test]
    fn max_results_truncates_via_default_search() {
        let (_t, view) = fixture();
        let engine = GrepSearchEngine::new();

        let mut q = SearchQuery::literal("fn");
        q.case = CaseMode::Insensitive;
        q.max_results = Some(2);
        let hits = engine.search(&view, &q).unwrap();
        // Many "fn" lines exist, but the buffered helper stops at 2.
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn streaming_stop_halts_immediately() {
        let (_t, view) = fixture();
        let engine = GrepSearchEngine::new();

        let mut q = SearchQuery::literal("fn");
        q.case = CaseMode::Insensitive;

        let mut count = 0;
        engine
            .search_streaming(&view, &q, &mut |_m| {
                count += 1;
                Flow::Stop // stop after the very first match
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn binary_files_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        // A "binary" file containing a NUL byte alongside the search term.
        fs::write(root.join("bin.dat"), b"hello\x00needle world").unwrap();
        fs::write(root.join("text.txt"), b"needle here\n").unwrap();
        let view = PhysicalFileView::new(root);
        let engine = GrepSearchEngine::new();

        let q = SearchQuery::literal("needle");
        let hits = sorted(engine.search(&view, &q).unwrap());
        // Only the text file matches; the binary file is detected and skipped.
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "text.txt");
    }

    #[test]
    fn submatch_offsets_are_correct() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        // Two occurrences of "ab" on one line: "xx ab yy ab".
        fs::write(root.join("f.txt"), "xx ab yy ab\n").unwrap();
        let view = PhysicalFileView::new(root);
        let engine = GrepSearchEngine::new();

        let mut q = SearchQuery::literal("ab");
        q.case = CaseMode::Sensitive;
        let hits = engine.search(&view, &q).unwrap();
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        // "xx ab yy ab" -> "ab" at [3,5) and [9,11).
        assert_eq!(h.submatches, vec![(3, 5), (9, 11)]);
        // Sanity-check that the offsets index the matched substring.
        for (s, e) in &h.submatches {
            assert_eq!(&h.line[*s as usize..*e as usize], "ab");
        }
    }

    #[test]
    fn line_field_strips_trailing_newline() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        fs::write(root.join("f.txt"), "alpha\r\nbeta\r\n").unwrap();
        let view = PhysicalFileView::new(root);
        let engine = GrepSearchEngine::new();

        let q = SearchQuery::literal("beta");
        let hits = engine.search(&view, &q).unwrap();
        assert_eq!(hits.len(), 1);
        // No trailing \r or \n in the reported line (CRLF input).
        assert_eq!(hits[0].line, "beta");
        assert_eq!(hits[0].line_no, 2);
    }

    #[test]
    fn no_match_yields_empty() {
        let (_t, view) = fixture();
        let engine = GrepSearchEngine::new();
        let q = SearchQuery::literal("zzz-not-present-zzz");
        let hits = engine.search(&view, &q).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn invalid_regex_is_a_search_error() {
        let (_t, view) = fixture();
        let engine = GrepSearchEngine::new();
        let mut q = SearchQuery::literal("("); // unbalanced group
        q.regex = true;
        let err = engine.search(&view, &q).unwrap_err();
        assert!(matches!(err, CoreError::Search(_)));
    }
}

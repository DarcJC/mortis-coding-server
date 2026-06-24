//! Whitelist filtering shared by the VCS backends.
//!
//! A [`GlobFilter`] compiles a repo's `include`/`exclude` globs once and decides
//! whether a given logical path (forward-slash, repo-relative) should be
//! materialized into the read-only working tree.

use camino::Utf8Path;
use globset::{Glob, GlobSet, GlobSetBuilder};
use mortis_core::{CoreError, Result};

/// Compiled include/exclude globs for whitelist materialization.
///
/// Semantics: a path is kept iff it matches `include` (or `include` is empty,
/// meaning "everything") AND does not match `exclude`.
#[derive(Debug, Default)]
pub struct GlobFilter {
    include: Option<GlobSet>,
    exclude: Option<GlobSet>,
}

impl GlobFilter {
    /// Compile the filter from raw glob patterns (e.g. `["src/**", "*.md"]`).
    pub fn new(include: &[String], exclude: &[String]) -> Result<Self> {
        Ok(Self {
            include: build(include)?,
            exclude: build(exclude)?,
        })
    }

    /// Whether `path` (forward-slash, repo-relative) passes the filter.
    pub fn matches(&self, path: &Utf8Path) -> bool {
        let s = path.as_str();
        if let Some(inc) = &self.include {
            if !inc.is_match(s) {
                return false;
            }
        }
        if let Some(exc) = &self.exclude {
            if exc.is_match(s) {
                return false;
            }
        }
        true
    }
}

fn build(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        let glob = Glob::new(p).map_err(|e| CoreError::Vcs(format!("invalid glob {p:?}: {e}")))?;
        builder.add(glob);
    }
    builder
        .build()
        .map(Some)
        .map_err(|e| CoreError::Vcs(format!("invalid glob set: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_include_keeps_everything() {
        let f = GlobFilter::new(&[], &[]).unwrap();
        assert!(f.matches(Utf8Path::new("anything/here.rs")));
    }

    #[test]
    fn include_then_exclude() {
        let f = GlobFilter::new(
            &["src/**".to_string(), "*.md".to_string()],
            &["**/*.bin".to_string()],
        )
        .unwrap();
        assert!(f.matches(Utf8Path::new("src/main.rs")));
        assert!(f.matches(Utf8Path::new("README.md")));
        assert!(!f.matches(Utf8Path::new("docs/guide.txt"))); // not included
        assert!(!f.matches(Utf8Path::new("src/blob.bin"))); // excluded
    }
}

//! The [`FileView`] abstraction — the seam between *where files live* and the
//! services that read and search them.
//!
//! A read-only repository working tree exposes a `FileView`; a session overlay
//! exposes a *different* `FileView` that layers per-session writes/deletes on
//! top of that same base. Because both resolve logical paths down to real
//! on-disk files, the search engine and file reader work identically against
//! either — they never need to know whether a session is involved.

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::Result;

/// A flat, read-oriented view of a set of files rooted at a logical directory.
///
/// Implementations are cheap to create and `Send + Sync` so they can be handed
/// to blocking search tasks.
pub trait FileView: Send + Sync {
    /// The logical root. Listed/resolved paths are relative to this.
    fn root(&self) -> &Utf8Path;

    /// Enumerate logical file paths, optionally restricted to a `subtree`.
    ///
    /// Whiteouts (session deletions) are already excluded; VCS metadata
    /// directories (`.git`, `.svn`) are never listed.
    fn list_files(&self, subtree: Option<&Utf8Path>) -> Result<Vec<Utf8PathBuf>>;

    /// Map a logical path to the actual on-disk file that backs it, or `None`
    /// if the path does not exist in this view (including whiteouts).
    fn resolve(&self, logical: &Utf8Path) -> Result<Option<Utf8PathBuf>>;

    /// Read the full byte content of a logical path.
    fn read(&self, logical: &Utf8Path) -> Result<Vec<u8>>;

    /// Whether a logical path exists (and is not whited-out) in this view.
    fn exists(&self, logical: &Utf8Path) -> bool {
        matches!(self.resolve(logical), Ok(Some(_)))
    }
}

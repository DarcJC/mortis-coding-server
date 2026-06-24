//! # mortis-fs
//!
//! Concrete [`FileView`](mortis_core::FileView) implementations:
//!
//! - [`PhysicalFileView`] — a plain read-only directory (a materialized repo
//!   working tree).
//! - [`OverlayFileView`] — a copy-on-write union: reads fall through to a
//!   read-only `base`, while an `upper` directory and a set of whiteouts
//!   (deletions) shadow it. This is what a session exposes for read/search.
//!
//! Both deliberately ignore version-control metadata directories (`.git`,
//! `.svn`) so they never leak into listings or searches.

use std::collections::{BTreeSet, HashSet};

use camino::{Utf8Path, Utf8PathBuf};
use mortis_core::{CoreError, FileView, Result};

/// Directory names that are never listed, resolved, or read.
const VCS_DIRS: &[&str] = &[".git", ".svn", ".hg"];

fn is_vcs_path(path: &Utf8Path) -> bool {
    path.components()
        .any(|c| VCS_DIRS.contains(&c.as_str()))
}

/// Whether a logical path would escape its root: any absolute, drive-prefix, or
/// `..` component. Such paths must never resolve to an on-disk file — otherwise
/// a request like `../../etc/passwd` would read outside the view. (Cheap and
/// allocation-free; the canonical normalizing form is
/// [`mortis_core::ensure_safe_relative`].)
fn escapes_root(path: &Utf8Path) -> bool {
    use camino::Utf8Component;
    path.components().any(|c| {
        matches!(
            c,
            Utf8Component::ParentDir | Utf8Component::RootDir | Utf8Component::Prefix(_)
        )
    })
}

/// Normalize a path to forward-slash form so logical paths are identical on
/// every platform (camino keeps the OS separator, which would otherwise leak
/// `\` into API responses on Windows).
fn to_logical(path: &Utf8Path) -> Utf8PathBuf {
    let s = path.as_str();
    if s.contains('\\') {
        Utf8PathBuf::from(s.replace('\\', "/"))
    } else {
        path.to_owned()
    }
}

/// Recursively collect file paths under `dir`, relative to `root`.
fn walk_into(root: &Utf8Path, dir: &Utf8Path, out: &mut BTreeSet<Utf8PathBuf>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let read = std::fs::read_dir(dir)?;
    for entry in read {
        let entry = entry?;
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|p| CoreError::Other(format!("non-utf8 path: {}", p.display())))?;
        let name = path.file_name().unwrap_or_default();
        if VCS_DIRS.contains(&name) {
            continue;
        }
        let ft = entry.file_type()?;
        if ft.is_dir() {
            walk_into(root, &path, out)?;
        } else if ft.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                out.insert(to_logical(rel));
            }
        }
        // Symlinks and other special files are intentionally skipped.
    }
    Ok(())
}

/// Whether `path` is within `subtree` (or `subtree` is `None`).
fn in_subtree(path: &Utf8Path, subtree: Option<&Utf8Path>) -> bool {
    match subtree {
        None => true,
        Some(s) if s.as_str().is_empty() || s == "." => true,
        Some(s) => path.starts_with(s),
    }
}

/// A read-only view over a real directory tree.
#[derive(Debug, Clone)]
pub struct PhysicalFileView {
    root: Utf8PathBuf,
}

impl PhysicalFileView {
    pub fn new(root: impl Into<Utf8PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl FileView for PhysicalFileView {
    fn root(&self) -> &Utf8Path {
        &self.root
    }

    fn list_files(&self, subtree: Option<&Utf8Path>) -> Result<Vec<Utf8PathBuf>> {
        let start = match subtree {
            Some(s) => self.root.join(s),
            None => self.root.clone(),
        };
        let mut set = BTreeSet::new();
        walk_into(&self.root, &start, &mut set)?;
        Ok(set.into_iter().collect())
    }

    fn resolve(&self, logical: &Utf8Path) -> Result<Option<Utf8PathBuf>> {
        if is_vcs_path(logical) || escapes_root(logical) {
            return Ok(None);
        }
        let p = self.root.join(logical);
        Ok(if p.is_file() { Some(p) } else { None })
    }

    fn read(&self, logical: &Utf8Path) -> Result<Vec<u8>> {
        match self.resolve(logical)? {
            Some(p) => Ok(std::fs::read(p)?),
            None => Err(CoreError::not_found(logical.as_str())),
        }
    }
}

/// A copy-on-write union view.
///
/// Resolution order for a logical path `p`:
/// 1. if `p` is whited-out → does not exist;
/// 2. else if `upper/p` is a file → serve from upper;
/// 3. else if `base/p` is a file → serve from base;
/// 4. else → does not exist.
#[derive(Debug, Clone)]
pub struct OverlayFileView {
    base: Utf8PathBuf,
    upper: Utf8PathBuf,
    deleted: HashSet<Utf8PathBuf>,
}

impl OverlayFileView {
    pub fn new(
        base: impl Into<Utf8PathBuf>,
        upper: impl Into<Utf8PathBuf>,
        deleted: HashSet<Utf8PathBuf>,
    ) -> Self {
        Self {
            base: base.into(),
            upper: upper.into(),
            deleted,
        }
    }

    fn is_deleted(&self, logical: &Utf8Path) -> bool {
        self.deleted.contains(logical)
    }
}

impl FileView for OverlayFileView {
    fn root(&self) -> &Utf8Path {
        &self.base
    }

    fn list_files(&self, subtree: Option<&Utf8Path>) -> Result<Vec<Utf8PathBuf>> {
        let mut set = BTreeSet::new();
        walk_into(&self.base, &self.base, &mut set)?;
        // Remove whiteouts, then union upper (which may re-add modified files).
        set.retain(|p| !self.deleted.contains(p));
        let mut upper_set = BTreeSet::new();
        walk_into(&self.upper, &self.upper, &mut upper_set)?;
        set.extend(upper_set);
        Ok(set
            .into_iter()
            .filter(|p| in_subtree(p, subtree))
            .collect())
    }

    fn resolve(&self, logical: &Utf8Path) -> Result<Option<Utf8PathBuf>> {
        if is_vcs_path(logical) || escapes_root(logical) || self.is_deleted(logical) {
            return Ok(None);
        }
        let up = self.upper.join(logical);
        if up.is_file() {
            return Ok(Some(up));
        }
        let bp = self.base.join(logical);
        Ok(if bp.is_file() { Some(bp) } else { None })
    }

    fn read(&self, logical: &Utf8Path) -> Result<Vec<u8>> {
        match self.resolve(logical)? {
            Some(p) => Ok(std::fs::read(p)?),
            None => Err(CoreError::not_found(logical.as_str())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn u(p: &std::path::Path) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(p.to_path_buf()).unwrap()
    }

    #[test]
    fn physical_lists_and_reads_skipping_vcs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = u(tmp.path());
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("src/a.rs"), b"fn a() {}").unwrap();
        fs::write(root.join("README.md"), b"# hi").unwrap();
        fs::write(root.join(".git/config"), b"x").unwrap();

        let view = PhysicalFileView::new(root.clone());
        let files = view.list_files(None).unwrap();
        assert_eq!(
            files,
            vec![
                Utf8PathBuf::from("README.md"),
                Utf8PathBuf::from("src/a.rs"),
            ]
        );
        assert_eq!(view.read(Utf8Path::new("src/a.rs")).unwrap(), b"fn a() {}");
        assert!(view.resolve(Utf8Path::new(".git/config")).unwrap().is_none());

        let scoped = view.list_files(Some(Utf8Path::new("src"))).unwrap();
        assert_eq!(scoped, vec![Utf8PathBuf::from("src/a.rs")]);
    }

    #[test]
    fn overlay_shadows_base_and_honors_whiteouts() {
        let tmp = tempfile::tempdir().unwrap();
        let base = u(&tmp.path().join("base"));
        let upper = u(&tmp.path().join("upper"));
        fs::create_dir_all(base.join("src")).unwrap();
        fs::create_dir_all(&upper).unwrap();
        fs::write(base.join("src/a.rs"), b"base-a").unwrap();
        fs::write(base.join("keep.txt"), b"keep").unwrap();
        fs::write(base.join("gone.txt"), b"gone").unwrap();
        // upper modifies a.rs and adds new.txt
        fs::create_dir_all(upper.join("src")).unwrap();
        fs::write(upper.join("src/a.rs"), b"upper-a").unwrap();
        fs::write(upper.join("new.txt"), b"new").unwrap();

        let mut deleted = HashSet::new();
        deleted.insert(Utf8PathBuf::from("gone.txt"));

        let view = OverlayFileView::new(base, upper, deleted);
        let files = view.list_files(None).unwrap();
        assert_eq!(
            files,
            vec![
                Utf8PathBuf::from("keep.txt"),
                Utf8PathBuf::from("new.txt"),
                Utf8PathBuf::from("src/a.rs"),
            ]
        );
        // a.rs served from upper
        assert_eq!(view.read(Utf8Path::new("src/a.rs")).unwrap(), b"upper-a");
        // gone.txt whited out
        assert!(view.resolve(Utf8Path::new("gone.txt")).unwrap().is_none());
        assert!(view.read(Utf8Path::new("gone.txt")).is_err());
        // keep.txt served from base
        assert_eq!(view.read(Utf8Path::new("keep.txt")).unwrap(), b"keep");
    }

    #[test]
    fn path_traversal_does_not_escape_the_view() {
        let tmp = tempfile::tempdir().unwrap();
        // A secret living *outside* both the base and upper roots.
        fs::write(tmp.path().join("secret.txt"), b"top secret").unwrap();
        let base = u(&tmp.path().join("base"));
        let upper = u(&tmp.path().join("upper"));
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&upper).unwrap();
        fs::write(base.join("ok.txt"), b"ok").unwrap();

        let physical = PhysicalFileView::new(base.clone());
        let overlay = OverlayFileView::new(base, upper, HashSet::new());

        for bad in ["../secret.txt", "../../secret.txt", "a/../../secret.txt"] {
            assert!(
                physical.resolve(Utf8Path::new(bad)).unwrap().is_none(),
                "physical resolved escaping path {bad:?}"
            );
            assert!(physical.read(Utf8Path::new(bad)).is_err());
            assert!(
                overlay.resolve(Utf8Path::new(bad)).unwrap().is_none(),
                "overlay resolved escaping path {bad:?}"
            );
            assert!(overlay.read(Utf8Path::new(bad)).is_err());
        }
        // ...while a legitimate in-root path still resolves.
        assert!(physical.resolve(Utf8Path::new("ok.txt")).unwrap().is_some());
    }
}

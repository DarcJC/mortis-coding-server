//! # mortis-embed
//!
//! Makes SVN support self-contained: per-OS `svn` distributions vendored under
//! `assets/svn/<os>-<arch>/` are embedded into the binary (via `rust-embed`),
//! extracted to a cache directory on first use, and run from there.
//!
//! Resolution order (see [`resolve_svn`]):
//! 1. an explicit operator-configured path,
//! 2. the embedded binary for the current platform (if vendored),
//! 3. a system `svn` found on `PATH`.
//!
//! If no platform binary is vendored (the default in source control — only
//! placeholders are committed), the server transparently falls back to a system
//! `svn`, so the build is never blocked on large binary blobs.

use camino::{Utf8Path, Utf8PathBuf};
use mortis_core::{CoreError, Result};
use rust_embed::RustEmbed;

/// The embedded `assets/` tree (svn distributions, when vendored).
#[derive(RustEmbed)]
#[folder = "assets/"]
struct Assets;

/// Where a resolved `svn` came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSource {
    /// Extracted from the embedded assets.
    Embedded,
    /// Found on the system `PATH`.
    System,
    /// An explicit operator-configured path.
    Override,
}

/// A resolved external tool: the program to run plus any environment it needs.
#[derive(Debug, Clone)]
pub struct SvnTool {
    /// Path to the `svn` executable.
    pub program: Utf8PathBuf,
    /// Extra environment variables (e.g. `PATH`/`LD_LIBRARY_PATH` additions).
    pub env: Vec<(String, String)>,
    /// Where it came from.
    pub source: ToolSource,
}

/// The platform tag used to select an embedded subdirectory, `"<os>-<arch>"`.
pub const fn platform_tag() -> &'static str {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "windows-x86_64"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-x86_64"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "linux-aarch64"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "macos-aarch64"
    } else {
        "unknown"
    }
}

/// The svn executable's path *relative to* an extracted platform directory.
fn exe_rel() -> &'static str {
    if cfg!(windows) { "svn.exe" } else { "bin/svn" }
}

/// A sensible per-user cache directory, if one can be determined.
pub fn default_cache_dir() -> Option<Utf8PathBuf> {
    directories::ProjectDirs::from("dev", "mortis", "mortis-code-server")
        .and_then(|d| Utf8PathBuf::from_path_buf(d.cache_dir().to_path_buf()).ok())
}

/// Resolve a usable `svn` tool, extracting the embedded copy if present.
pub fn resolve_svn(cache_dir: &Utf8Path, override_path: Option<&Utf8Path>) -> Result<SvnTool> {
    if let Some(p) = override_path {
        if p.is_file() {
            return Ok(SvnTool { program: p.to_owned(), env: Vec::new(), source: ToolSource::Override });
        }
        return Err(CoreError::Config(format!("configured svn binary not found: {p}")));
    }

    if let Some(tool) = extract_embedded(cache_dir)? {
        tracing::info!("using embedded svn at {}", tool.program);
        return Ok(tool);
    }

    if let Some(program) = which_svn() {
        tracing::info!("using system svn at {program}");
        return Ok(SvnTool { program, env: Vec::new(), source: ToolSource::System });
    }

    Err(CoreError::Config(
        "no svn binary available: none vendored for this platform, none configured, \
         and none found on PATH"
            .into(),
    ))
}

/// Extract the embedded svn for this platform, returning `None` if none is
/// vendored (only placeholders) so the caller can fall back to the system tool.
fn extract_embedded(cache_dir: &Utf8Path) -> Result<Option<SvnTool>> {
    let tag = platform_tag();
    let prefix = format!("svn/{tag}/");

    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for path in Assets::iter() {
        let Some(rel) = path.strip_prefix(&prefix) else { continue };
        if rel.is_empty() || rel.ends_with(".gitkeep") {
            continue;
        }
        if let Some(file) = Assets::get(&path) {
            files.push((rel.to_string(), file.data.into_owned()));
        }
    }
    if files.is_empty() {
        return Ok(None);
    }

    let dest = cache_dir.join(format!("svn-{tag}"));
    materialize(&files, &dest)?;

    let program = dest.join(exe_rel());
    if !program.is_file() {
        // Something is vendored but not a usable svn — fall back.
        return Ok(None);
    }
    Ok(Some(SvnTool {
        program,
        env: tool_env(&dest),
        source: ToolSource::Embedded,
    }))
}

/// Per-platform environment so the extracted svn finds its shared libraries.
fn tool_env(dir: &Utf8Path) -> Vec<(String, String)> {
    if cfg!(windows) {
        // DLLs sit beside svn.exe; prepend the dir to PATH.
        let existing = std::env::var("PATH").unwrap_or_default();
        vec![("PATH".to_string(), format!("{dir};{existing}"))]
    } else {
        let lib = dir.join("lib");
        let existing = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
        vec![("LD_LIBRARY_PATH".to_string(), format!("{lib}:{existing}"))]
    }
}

/// Write `files` (relative path → bytes) under `dest`, marking them executable
/// on Unix. Exposed for testing the extraction mechanism without real binaries.
pub fn materialize(files: &[(String, Vec<u8>)], dest: &Utf8Path) -> Result<()> {
    for (rel, data) in files {
        let path = dest.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(&path)?.permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(&path, perm)?;
        }
    }
    Ok(())
}

/// Search `PATH` for an `svn` executable.
fn which_svn() -> Option<Utf8PathBuf> {
    let name = if cfg!(windows) { "svn.exe" } else { "svn" };
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            if let Ok(utf8) = Utf8PathBuf::from_path_buf(candidate) {
                return Some(utf8);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materialize_writes_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = Utf8PathBuf::from_path_buf(tmp.path().join("out")).unwrap();
        let files = vec![
            ("bin/svn".to_string(), b"#!/bin/sh\necho fake".to_vec()),
            ("lib/libfake.so".to_string(), b"\x7fELF".to_vec()),
        ];
        materialize(&files, &dest).unwrap();
        assert!(dest.join("bin/svn").is_file());
        assert!(dest.join("lib/libfake.so").is_file());
        assert_eq!(std::fs::read(dest.join("bin/svn")).unwrap(), b"#!/bin/sh\necho fake");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dest.join("bin/svn")).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "executable bit must be set");
        }
    }

    #[test]
    fn no_vendored_binary_falls_back_or_errors() {
        // With only placeholders committed, embedded extraction yields nothing,
        // so resolution depends solely on a system svn. Either outcome is valid;
        // we only assert it does not panic and reports a coherent source.
        let tmp = tempfile::tempdir().unwrap();
        let cache = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        match resolve_svn(&cache, None) {
            Ok(tool) => assert_ne!(tool.source, ToolSource::Override),
            Err(e) => assert_eq!(e.code(), "config_error"),
        }
    }

    #[test]
    fn override_path_is_honored() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = Utf8PathBuf::from_path_buf(tmp.path().join("myforsvn")).unwrap();
        std::fs::write(&fake, b"x").unwrap();
        let cache = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let tool = resolve_svn(&cache, Some(&fake)).unwrap();
        assert_eq!(tool.source, ToolSource::Override);
        assert_eq!(tool.program, fake);
    }
}

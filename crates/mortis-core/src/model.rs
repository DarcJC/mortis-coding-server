//! Core value types: identifiers, revisions, read ranges, file content, and a
//! serialization-friendly timestamp.

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// Stable identifier of a configured repository (e.g. `"proj-a"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoId(pub String);

impl fmt::Display for RepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for RepoId {
    fn from(s: &str) -> Self {
        RepoId(s.to_owned())
    }
}

impl From<String> for RepoId {
    fn from(s: String) -> Self {
        RepoId(s)
    }
}

/// Opaque session identifier (a UUID rendered as a string).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> Self {
        SessionId(s.to_owned())
    }
}

/// The authenticated principal that owns sessions and issues requests.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Principal(pub String);

impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for Principal {
    fn from(s: &str) -> Self {
        Principal(s.to_owned())
    }
}

/// A revision selector, interpreted by the concrete VCS backend.
///
/// For Git the string is any revspec (branch, tag, or commit-ish); for SVN it
/// is a revision number or keyword (e.g. `"HEAD"`, `"1234"`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rev {
    /// The current default head of the materialized working tree.
    #[default]
    Head,
    /// A specific, backend-interpreted revision.
    At(String),
}

impl Rev {
    /// Build a [`Rev`] from an optional string (`None` → [`Rev::Head`]).
    pub fn from_opt(s: Option<String>) -> Self {
        match s {
            Some(s) if !s.is_empty() && s != "HEAD" => Rev::At(s),
            _ => Rev::Head,
        }
    }
}

/// A bounded slice of a file to read. Omitting a range reads the whole file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "unit", rename_all = "snake_case")]
pub enum ReadRange {
    /// 1-based, inclusive line range. `end == None` reads to end of file.
    Lines { start: u32, end: Option<u32> },
    /// 0-based byte range. `end == None` reads to end of file.
    Bytes { start: u64, end: Option<u64> },
}

/// The result of a file read, with enough metadata for clients to paginate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileContent {
    /// Logical path relative to the repo/session root.
    pub path: Utf8PathBuf,
    /// The returned text (lossy UTF-8 for non-UTF-8 inputs).
    pub text: String,
    /// 1-based first line included (1 when whole file or byte range).
    pub start_line: u32,
    /// 1-based last line included.
    pub end_line: u32,
    /// Total number of lines in the underlying file.
    pub total_lines: u32,
    /// Whether the returned slice is a subset of the file.
    pub truncated: bool,
    /// Whether the file was detected as binary.
    pub is_binary: bool,
}

/// A serialization-friendly timestamp: Unix epoch milliseconds.
///
/// We avoid `SystemTime` in serialized types because its serde representation
/// is awkward and not stable across platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Timestamp(pub u64);

impl Timestamp {
    /// Current wall-clock time.
    pub fn now() -> Self {
        Timestamp::from_system(SystemTime::now())
    }

    /// Convert from a [`SystemTime`], saturating pre-epoch values to 0.
    pub fn from_system(t: SystemTime) -> Self {
        let millis = t
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Timestamp(millis)
    }

    /// Convert from a Unix epoch in seconds.
    pub fn from_unix_secs(secs: i64) -> Self {
        Timestamp((secs.max(0) as u64) * 1000)
    }

    /// Back to a [`SystemTime`].
    pub fn to_system(self) -> SystemTime {
        UNIX_EPOCH + std::time::Duration::from_millis(self.0)
    }
}

/// Heuristic binary detection: a NUL byte within the first 8 KiB.
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

/// Count lines using git's convention: a trailing newline does not add an
/// empty final line (so `"a\n"` is one line, `"a\nb"` is two).
fn count_lines(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    let newlines = text.matches('\n').count() as u32;
    if text.ends_with('\n') {
        newlines
    } else {
        newlines + 1
    }
}

/// Build a [`FileContent`] from raw bytes, applying an optional [`ReadRange`].
///
/// This is the single shared implementation used both when reading a blob at a
/// revision (the VCS backends) and when reading through a session view (the
/// file service), so range/line-count semantics stay identical everywhere.
pub fn slice_file_content(
    path: Utf8PathBuf,
    bytes: &[u8],
    range: Option<ReadRange>,
) -> FileContent {
    let is_binary = looks_binary(bytes);
    let full = String::from_utf8_lossy(bytes);
    let total_lines = count_lines(&full);

    match range {
        None => FileContent {
            path,
            start_line: if total_lines == 0 { 0 } else { 1 },
            end_line: total_lines,
            total_lines,
            truncated: false,
            is_binary,
            text: full.into_owned(),
        },
        Some(ReadRange::Lines { start, end }) => {
            // 1-based inclusive; `split('\n')` may yield a trailing "" for a
            // file ending in '\n', which is naturally excluded by `total_lines`.
            let lines: Vec<&str> = full.split('\n').collect();
            let start = start.max(1);
            let end = end.unwrap_or(total_lines).min(total_lines);
            if total_lines == 0 || start > end {
                return FileContent {
                    path,
                    text: String::new(),
                    start_line: start,
                    end_line: start.saturating_sub(1),
                    total_lines,
                    truncated: total_lines > 0,
                    is_binary,
                };
            }
            let slice = &lines[(start as usize - 1)..(end as usize)];
            FileContent {
                path,
                text: slice.join("\n"),
                start_line: start,
                end_line: end,
                total_lines,
                truncated: start > 1 || end < total_lines,
                is_binary,
            }
        }
        Some(ReadRange::Bytes { start, end }) => {
            let len = bytes.len();
            let start = (start as usize).min(len);
            let end = end.map(|e| (e as usize).min(len)).unwrap_or(len).max(start);
            let slice = &bytes[start..end];
            let start_line = count_lines(&String::from_utf8_lossy(&bytes[..start])) + 1;
            let text = String::from_utf8_lossy(slice).into_owned();
            let end_line = start_line + count_lines(&text).saturating_sub(1);
            FileContent {
                path,
                text,
                start_line,
                end_line,
                total_lines,
                truncated: start > 0 || end < len,
                is_binary,
            }
        }
    }
}

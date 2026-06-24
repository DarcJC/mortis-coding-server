//! The [`AssemblyStore`] port — assembly-query sessions over a downloaded
//! binary.
//!
//! An assembly session downloads a (temporary) binary from a URL, validates it
//! is a recognized executable format, and then answers low-level queries:
//! disassemble a virtual-address range, resolve an address to a function name,
//! and report header/section metadata. The lifecycle (create → download →
//! validate → ready/failed) is observable through [`AsmSession::status`].
//!
//! Like [`crate::session::SessionStore`], sessions are owner-scoped: the store
//! tags each session with its creating [`Principal`] and the service layer
//! refuses cross-owner access.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::error::Result;
use crate::model::{Principal, Timestamp};

/// Opaque assembly-session identifier (a UUID rendered as a string).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AsmSessionId(pub String);

impl fmt::Display for AsmSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for AsmSessionId {
    fn from(s: &str) -> Self {
        AsmSessionId(s.to_owned())
    }
}

/// The operating-system family a binary targets.
///
/// `Linux` also covers Android (both are ELF); `Apple` covers macOS and iOS
/// (both are Mach-O).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BinaryOs {
    Windows,
    Linux,
    Apple,
}

/// The container format of a recognized binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BinaryFormat {
    /// Windows Portable Executable.
    Pe,
    /// ELF (Linux/Android).
    Elf,
    /// Single-architecture Mach-O (macOS/iOS).
    MachO,
    /// Multi-architecture ("fat"/"universal") Mach-O.
    MachOFat,
}

/// One section of a binary (named, with its virtual address and on-disk range).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SectionInfo {
    pub name: String,
    /// Virtual address the section is mapped at.
    pub address: u64,
    /// Size in memory.
    pub size: u64,
    /// Offset of the section's bytes within the file (0 if not file-backed).
    pub file_offset: u64,
}

/// One loadable segment of a binary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentInfo {
    pub name: String,
    /// Virtual address the segment is mapped at.
    pub address: u64,
    /// Size in memory.
    pub size: u64,
    /// Offset of the segment's bytes within the file.
    pub file_offset: u64,
    /// Number of bytes backed by the file (may be less than `size`, e.g. .bss).
    pub file_size: u64,
}

/// Header/metadata summary of a validated binary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryInfo {
    pub format: BinaryFormat,
    pub os: BinaryOs,
    /// Architecture name (e.g. `"x86_64"`, `"aarch64"`, `"arm"`, `"i386"`).
    pub arch: String,
    /// Pointer width in bits (32 or 64).
    pub bits: u8,
    pub little_endian: bool,
    /// Entry-point virtual address (0 if not applicable).
    pub entry: u64,
    pub sections: Vec<SectionInfo>,
    pub segments: Vec<SegmentInfo>,
    pub symbol_count: usize,
    pub import_count: usize,
    pub export_count: usize,
    /// For fat Mach-O: the architectures present (e.g. `["x86_64","arm64"]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sub_archs: Vec<String>,
}

/// The lifecycle state of an assembly session, as reported by a status query.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AsmStatus {
    /// The binary is still downloading. `total` is `None` until the server
    /// reports a `Content-Length`.
    Downloading { downloaded: u64, total: Option<u64> },
    /// Download finished; the binary is being parsed/validated.
    Validating,
    /// The binary is downloaded and validated; queries are available.
    Ready { info: BinaryInfo },
    /// The session failed. `code` is a stable machine-readable category (e.g.
    /// `"too_large"`, `"invalid_binary"`, `"download_failed"`).
    Failed { code: String, message: String },
}

/// A public, owner-scoped view of one assembly session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsmSession {
    pub id: AsmSessionId,
    pub owner: Principal,
    /// The URL the binary was (or is being) downloaded from.
    pub source_url: String,
    pub created: Timestamp,
    pub last_accessed: Timestamp,
    pub status: AsmStatus,
}

/// One disassembled instruction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instruction {
    /// Virtual address of the instruction.
    pub address: u64,
    /// Raw instruction bytes, hex-encoded (e.g. `"4889e5"`).
    pub bytes: String,
    /// Mnemonic (e.g. `"mov"`).
    pub mnemonic: String,
    /// Operand text (e.g. `"rbp, rsp"`).
    pub operands: String,
}

/// The result of disassembling an address range.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Disassembly {
    /// The virtual address the disassembly starts at.
    pub start: u64,
    pub instructions: Vec<Instruction>,
}

/// The result of resolving an address to a function symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionResolution {
    /// The queried address.
    pub address: u64,
    /// Resolved function name, or `None` for a stripped binary / no match.
    pub name: Option<String>,
    /// Start address of the matched symbol, if any.
    pub symbol_start: Option<u64>,
    /// `address - symbol_start` when a symbol matched.
    pub offset: Option<u64>,
    /// Whether `address` fell strictly within the symbol's `[start, start+size)`
    /// range (true) or was merely the nearest preceding symbol (false).
    pub exact: bool,
}

/// Operator policy controlling binary downloads.
///
/// `allowed_hosts` is deny-by-default: an empty list rejects every download.
/// Each entry matches a URL host case-insensitively, either exactly
/// (`example.com`) or — when written with a leading dot (`.example.com`) — that
/// domain and any subdomain.
#[derive(Debug, Clone)]
pub struct AsmDownloadPolicy {
    pub allowed_hosts: Vec<String>,
    pub max_download_bytes: u64,
    pub timeout: Duration,
    pub max_sessions: usize,
}

impl AsmDownloadPolicy {
    /// Whether `host` is permitted by the allowlist (deny-by-default).
    pub fn host_allowed(&self, host: &str) -> bool {
        let host = host.trim().to_ascii_lowercase();
        self.allowed_hosts.iter().any(|entry| {
            let entry = entry.trim().to_ascii_lowercase();
            if let Some(domain) = entry.strip_prefix('.') {
                host == domain || host.ends_with(&format!(".{domain}"))
            } else {
                host == entry
            }
        })
    }
}

/// The assembly-query session store: download + validate + low-level queries.
///
/// `create` returns immediately with the session in [`AsmStatus::Downloading`];
/// the download and validation proceed in the background and are observed via
/// [`AssemblyStore::get`]. The query methods (`disassemble`, `resolve_function`,
/// `metadata`) require the session to have reached [`AsmStatus::Ready`].
#[async_trait]
pub trait AssemblyStore: Send + Sync {
    /// Begin a session: validate `url` against policy and kick off the download.
    /// Returns immediately (status = downloading).
    async fn create(&self, owner: &Principal, url: &str) -> Result<AsmSession>;

    /// Fetch a session (including its current status/progress).
    async fn get(&self, id: &AsmSessionId) -> Result<AsmSession>;

    /// List all sessions owned by `owner`.
    async fn list(&self, owner: &Principal) -> Result<Vec<AsmSession>>;

    /// Delete a session and its downloaded artifact.
    async fn delete(&self, id: &AsmSessionId) -> Result<()>;

    /// Disassemble `len` bytes starting at virtual address `start`.
    async fn disassemble(&self, id: &AsmSessionId, start: u64, len: u64) -> Result<Disassembly>;

    /// Resolve a virtual address to the function symbol containing it.
    async fn resolve_function(
        &self,
        id: &AsmSessionId,
        address: u64,
    ) -> Result<FunctionResolution>;

    /// Header/section metadata for the validated binary.
    async fn metadata(&self, id: &AsmSessionId) -> Result<BinaryInfo>;

    /// Refresh `last_accessed` to now.
    async fn touch(&self, id: &AsmSessionId) -> Result<()>;

    /// Delete every session idle for longer than `ttl`. Returns the count.
    async fn reap_expired(&self, ttl: Duration) -> Result<usize>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_allowlist_exact_and_subdomain() {
        let policy = AsmDownloadPolicy {
            allowed_hosts: vec!["example.com".into(), ".cdn.net".into()],
            max_download_bytes: 1,
            timeout: Duration::from_secs(1),
            max_sessions: 1,
        };
        // Exact match (case-insensitive).
        assert!(policy.host_allowed("example.com"));
        assert!(policy.host_allowed("EXAMPLE.com"));
        assert!(!policy.host_allowed("evil.example.com")); // exact entry, no subdomains
        // Leading-dot entry matches the domain and its subdomains.
        assert!(policy.host_allowed("cdn.net"));
        assert!(policy.host_allowed("a.cdn.net"));
        assert!(!policy.host_allowed("cdn.net.evil.com"));
        // Unlisted host is denied.
        assert!(!policy.host_allowed("other.org"));
    }

    #[test]
    fn empty_allowlist_denies_everything() {
        let policy = AsmDownloadPolicy {
            allowed_hosts: vec![],
            max_download_bytes: 1,
            timeout: Duration::from_secs(1),
            max_sessions: 1,
        };
        assert!(!policy.host_allowed("example.com"));
    }
}

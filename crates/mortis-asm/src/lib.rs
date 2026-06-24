//! # mortis-asm
//!
//! An in-memory [`AssemblyStore`](mortis_core::AssemblyStore): create a session
//! from a URL, download the binary asynchronously (tracking progress), validate
//! it with [`object`], and answer disassembly / symbol / metadata queries with
//! [`capstone`] + [`object`].
//!
//! ## Why in-memory?
//!
//! CoW edit sessions are persisted because user *edits* must survive a restart.
//! Assembly sessions are *derived, reconstructible caches*: the only durable
//! input is the URL, and an in-flight HTTP stream cannot resume across a restart
//! anyway. So the registry lives in memory (`RwLock<HashMap<…>>`) and only the
//! downloaded bytes are written to disk under `<download_dir>/<id>/`. After a
//! restart, sessions are simply gone and clients re-create them.
//!
//! ## Download safety
//!
//! Downloads are gated by an operator [`AsmDownloadPolicy`](mortis_core::asm::AsmDownloadPolicy):
//! a deny-by-default host allowlist, a maximum size (the stream is aborted and
//! the partial file removed if exceeded), a request timeout, and an `http`/
//! `https`-only scheme check. Redirects are disabled so an allowlisted host
//! cannot bounce the download to a non-allowlisted target.

mod binary;

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use camino::Utf8PathBuf;
use futures::StreamExt;
use tokio::io::AsyncWriteExt;
use url::Url;

use mortis_core::asm::{
    AsmDownloadPolicy, AsmSession, AsmSessionId, AsmStatus, AssemblyStore, BinaryInfo, Disassembly,
    FunctionResolution,
};
use mortis_core::error::{CoreError, Result};
use mortis_core::model::{Principal, Timestamp};

const PHASE_DOWNLOADING: u8 = 0;
const PHASE_VALIDATING: u8 = 1;
const PHASE_READY: u8 = 2;
const PHASE_FAILED: u8 = 3;

/// Terminal result of a session, stored once it reaches ready/failed.
enum Outcome {
    Ready(BinaryInfo),
    Failed { code: String, message: String },
}

/// Live, mutable state for one assembly session.
///
/// Download progress (`downloaded`/`total`) is tracked with atomics so a status
/// query never blocks the downloading task; the terminal [`Outcome`] is written
/// once under a short-lived mutex.
struct AsmSessionState {
    owner: Principal,
    source_url: String,
    created: Timestamp,
    /// `<download_dir>/<id>` — removed wholesale on delete/reap.
    dir: Utf8PathBuf,
    /// The downloaded binary's path on disk.
    file_path: Utf8PathBuf,
    last_accessed: AtomicU64,
    downloaded: AtomicU64,
    /// Total size from `Content-Length`; `-1` until known.
    total: AtomicI64,
    phase: AtomicU8,
    outcome: Mutex<Option<Outcome>>,
}

impl AsmSessionState {
    fn touch(&self) {
        self.last_accessed.store(Timestamp::now().0, Ordering::SeqCst);
    }

    fn set_failed(&self, code: &str, message: String) {
        *self.outcome.lock().unwrap() = Some(Outcome::Failed {
            code: code.to_string(),
            message,
        });
        self.phase.store(PHASE_FAILED, Ordering::SeqCst);
    }

    fn set_ready(&self, info: BinaryInfo) {
        *self.outcome.lock().unwrap() = Some(Outcome::Ready(info));
        self.phase.store(PHASE_READY, Ordering::SeqCst);
    }

    fn status(&self) -> AsmStatus {
        match self.phase.load(Ordering::SeqCst) {
            PHASE_DOWNLOADING => {
                let total = self.total.load(Ordering::SeqCst);
                AsmStatus::Downloading {
                    downloaded: self.downloaded.load(Ordering::SeqCst),
                    total: if total < 0 { None } else { Some(total as u64) },
                }
            }
            PHASE_VALIDATING => AsmStatus::Validating,
            _ => match &*self.outcome.lock().unwrap() {
                Some(Outcome::Ready(info)) => AsmStatus::Ready { info: info.clone() },
                Some(Outcome::Failed { code, message }) => AsmStatus::Failed {
                    code: code.clone(),
                    message: message.clone(),
                },
                // Phase flipped but the outcome write hasn't landed yet.
                None => AsmStatus::Validating,
            },
        }
    }

    fn to_session(&self, id: AsmSessionId) -> AsmSession {
        AsmSession {
            id,
            owner: self.owner.clone(),
            source_url: self.source_url.clone(),
            created: self.created,
            last_accessed: Timestamp(self.last_accessed.load(Ordering::SeqCst)),
            status: self.status(),
        }
    }
}

/// An in-memory assembly-session store backed by on-disk downloaded binaries.
pub struct MemAssemblyStore {
    download_dir: Utf8PathBuf,
    policy: AsmDownloadPolicy,
    http: reqwest::Client,
    sessions: Arc<RwLock<HashMap<AsmSessionId, Arc<AsmSessionState>>>>,
}

impl MemAssemblyStore {
    /// Create a store rooted at `download_dir` (created if missing).
    pub fn new(download_dir: impl Into<Utf8PathBuf>, policy: AsmDownloadPolicy) -> Result<Self> {
        let download_dir = download_dir.into();
        std::fs::create_dir_all(&download_dir)?;
        let http = reqwest::Client::builder()
            .timeout(policy.timeout)
            // No redirects: an allowlisted host must not be able to 302 the
            // download to a non-allowlisted target.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| CoreError::Other(format!("failed to build http client: {e}")))?;
        Ok(Self {
            download_dir,
            policy,
            http,
            sessions: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    fn get_state(&self, id: &AsmSessionId) -> Result<Arc<AsmSessionState>> {
        self.sessions
            .read()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| CoreError::not_found(id.0.clone()))
    }

    /// Touch and require the session to be `Ready`, then read its bytes off the
    /// blocking pool.
    async fn ready_bytes(&self, id: &AsmSessionId) -> Result<Vec<u8>> {
        let state = self.get_state(id)?;
        state.touch();
        if state.phase.load(Ordering::SeqCst) != PHASE_READY {
            return Err(CoreError::Conflict("binary is not ready for queries".into()));
        }
        let path = state.file_path.clone();
        tokio::task::spawn_blocking(move || std::fs::read(&path))
            .await
            .map_err(|e| CoreError::Other(format!("blocking read failed: {e}")))?
            .map_err(CoreError::from)
    }
}

#[async_trait]
impl AssemblyStore for MemAssemblyStore {
    async fn create(&self, owner: &Principal, url: &str) -> Result<AsmSession> {
        // Cap concurrent sessions.
        {
            let map = self.sessions.read().unwrap();
            if map.len() >= self.policy.max_sessions {
                return Err(CoreError::Conflict(format!(
                    "too many assembly sessions (max {})",
                    self.policy.max_sessions
                )));
            }
        }

        let parsed = Url::parse(url).map_err(|e| CoreError::invalid(format!("invalid url: {e}")))?;
        match parsed.scheme() {
            "http" | "https" => {}
            other => {
                return Err(CoreError::invalid(format!(
                    "unsupported url scheme '{other}' (only http/https)"
                )));
            }
        }
        let host = parsed
            .host_str()
            .ok_or_else(|| CoreError::invalid("url has no host"))?;
        if !self.policy.host_allowed(host) {
            return Err(CoreError::Forbidden(if self.policy.allowed_hosts.is_empty() {
                "binary downloads are disabled (no [asm].allowed_hosts configured)".into()
            } else {
                format!("download host not allowed: {host}")
            }));
        }

        let id = AsmSessionId(uuid::Uuid::new_v4().to_string());
        let dir = self.download_dir.join(&id.0);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(CoreError::from)?;
        let file_path = dir.join(filename_from_url(&parsed));
        let now = Timestamp::now();

        let state = Arc::new(AsmSessionState {
            owner: owner.clone(),
            source_url: url.to_string(),
            created: now,
            dir,
            file_path,
            last_accessed: AtomicU64::new(now.0),
            downloaded: AtomicU64::new(0),
            total: AtomicI64::new(-1),
            phase: AtomicU8::new(PHASE_DOWNLOADING),
            outcome: Mutex::new(None),
        });
        self.sessions
            .write()
            .unwrap()
            .insert(id.clone(), state.clone());

        // Kick off the download in the background; return immediately.
        let http = self.http.clone();
        let max = self.policy.max_download_bytes;
        let task_state = state.clone();
        tokio::spawn(async move {
            download_into(http, parsed, max, task_state).await;
        });

        Ok(state.to_session(id))
    }

    async fn get(&self, id: &AsmSessionId) -> Result<AsmSession> {
        let state = self.get_state(id)?;
        state.touch();
        Ok(state.to_session(id.clone()))
    }

    async fn list(&self, owner: &Principal) -> Result<Vec<AsmSession>> {
        let map = self.sessions.read().unwrap();
        let mut out: Vec<AsmSession> = map
            .iter()
            .filter(|(_, st)| &st.owner == owner)
            .map(|(id, st)| st.to_session(id.clone()))
            .collect();
        out.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        Ok(out)
    }

    async fn delete(&self, id: &AsmSessionId) -> Result<()> {
        let state = self.sessions.write().unwrap().remove(id);
        match state {
            Some(st) => {
                let _ = tokio::fs::remove_dir_all(&st.dir).await;
                Ok(())
            }
            None => Err(CoreError::not_found(id.0.clone())),
        }
    }

    async fn disassemble(&self, id: &AsmSessionId, start: u64, len: u64) -> Result<Disassembly> {
        let bytes = self.ready_bytes(id).await?;
        let instructions =
            tokio::task::spawn_blocking(move || binary::disassemble_range(&bytes, start, len))
                .await
                .map_err(|e| CoreError::Other(format!("disassembly task failed: {e}")))??;
        Ok(Disassembly { start, instructions })
    }

    async fn resolve_function(
        &self,
        id: &AsmSessionId,
        address: u64,
    ) -> Result<FunctionResolution> {
        let bytes = self.ready_bytes(id).await?;
        tokio::task::spawn_blocking(move || binary::resolve_function(&bytes, address))
            .await
            .map_err(|e| CoreError::Other(format!("resolve task failed: {e}")))?
    }

    async fn metadata(&self, id: &AsmSessionId) -> Result<BinaryInfo> {
        let state = self.get_state(id)?;
        state.touch();
        match &*state.outcome.lock().unwrap() {
            Some(Outcome::Ready(info)) => Ok(info.clone()),
            Some(Outcome::Failed { message, .. }) => {
                Err(CoreError::Conflict(format!("binary is not ready: {message}")))
            }
            None => Err(CoreError::Conflict("binary is not ready for queries".into())),
        }
    }

    async fn touch(&self, id: &AsmSessionId) -> Result<()> {
        self.get_state(id)?.touch();
        Ok(())
    }

    async fn reap_expired(&self, ttl: Duration) -> Result<usize> {
        let now = Timestamp::now().0;
        let ttl_ms = ttl.as_millis() as u64;
        let expired: Vec<(AsmSessionId, Arc<AsmSessionState>)> = {
            let map = self.sessions.read().unwrap();
            map.iter()
                .filter(|(_, st)| {
                    now.saturating_sub(st.last_accessed.load(Ordering::SeqCst)) > ttl_ms
                })
                .map(|(id, st)| (id.clone(), st.clone()))
                .collect()
        };
        let mut reaped = 0;
        for (id, st) in expired {
            self.sessions.write().unwrap().remove(&id);
            let _ = tokio::fs::remove_dir_all(&st.dir).await;
            reaped += 1;
        }
        Ok(reaped)
    }
}

/// Run the download and record success/failure on the session state.
async fn download_into(
    http: reqwest::Client,
    url: Url,
    max_bytes: u64,
    state: Arc<AsmSessionState>,
) {
    if let Err((code, message)) = try_download(&http, url, max_bytes, &state).await {
        // Remove any partial artifact so a failed/oversized download leaves no
        // bytes on disk.
        let _ = tokio::fs::remove_file(&state.file_path).await;
        state.set_failed(&code, message);
    }
}

/// The fallible download body. Returns `(error_code, message)` on failure.
async fn try_download(
    http: &reqwest::Client,
    url: Url,
    max_bytes: u64,
    state: &AsmSessionState,
) -> std::result::Result<(), (String, String)> {
    let resp = http
        .get(url)
        .send()
        .await
        .map_err(|e| ("download_failed".to_string(), e.to_string()))?;
    if !resp.status().is_success() {
        return Err((
            "download_failed".to_string(),
            format!("server returned HTTP {}", resp.status().as_u16()),
        ));
    }
    if let Some(len) = resp.content_length() {
        state.total.store(len as i64, Ordering::SeqCst);
        if len > max_bytes {
            return Err((
                "too_large".to_string(),
                format!("content-length {len} exceeds limit {max_bytes}"),
            ));
        }
    }

    let mut file = tokio::fs::File::create(&state.file_path)
        .await
        .map_err(|e| ("io_error".to_string(), e.to_string()))?;
    let mut downloaded: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ("download_failed".to_string(), e.to_string()))?;
        downloaded += chunk.len() as u64;
        if downloaded > max_bytes {
            return Err((
                "too_large".to_string(),
                format!("download exceeded size limit {max_bytes}"),
            ));
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| ("io_error".to_string(), e.to_string()))?;
        state.downloaded.store(downloaded, Ordering::SeqCst);
    }
    file.flush()
        .await
        .map_err(|e| ("io_error".to_string(), e.to_string()))?;
    drop(file);

    // Validate on the blocking pool (object parsing is CPU/IO, not async).
    state.phase.store(PHASE_VALIDATING, Ordering::SeqCst);
    let path = state.file_path.clone();
    let info = tokio::task::spawn_blocking(move || -> Result<BinaryInfo> {
        let bytes = std::fs::read(&path)?;
        binary::detect_and_describe(&bytes)
    })
    .await
    .map_err(|e| ("internal_error".to_string(), format!("validation task failed: {e}")))?
    .map_err(|e| ("invalid_binary".to_string(), e.to_string()))?;
    state.set_ready(info);
    Ok(())
}

/// Derive a safe on-disk filename from a URL's last path segment, falling back
/// to `artifact.bin`. Any separators / `.`/`..` are rejected (the id-based
/// parent directory already isolates downloads; this just keeps names sane).
fn filename_from_url(url: &Url) -> String {
    let raw = url
        .path_segments()
        .and_then(|mut seg| seg.next_back())
        .unwrap_or("")
        .trim();
    if raw.is_empty() || raw == "." || raw == ".." || raw.contains('/') || raw.contains('\\') {
        "artifact.bin".to_string()
    } else {
        raw.to_string()
    }
}

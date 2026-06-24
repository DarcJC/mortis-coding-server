//! Streamable-HTTP MCP adapter.
//!
//! Each `#[tool]` is a thin shim over [`Services`] — the same methods the REST
//! handlers call. The authenticated [`Principal`] is recovered from the HTTP
//! request parts that rmcp injects into the tool context (our auth middleware
//! placed it there before the request reached this service).

use std::sync::Arc;

use axum::http::request::Parts;
use camino::{Utf8Path, Utf8PathBuf};
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::{
    StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use mortis_app::Services;
use mortis_core::{
    AsmSessionId, CaseMode, LogQuery, Principal, RepoId, Rev, SearchQuery, SessionId, line_range,
};

use crate::error::to_mcp_error;
use crate::state::AppState;

/// The MCP server handler. Cheap to clone (just an `Arc`).
#[derive(Clone)]
pub struct McpServer {
    services: Arc<Services>,
}

impl McpServer {
    pub fn new(services: Arc<Services>) -> Self {
        Self { services }
    }
}

/// Build the Streamable-HTTP MCP tower service to mount at `/mcp`.
///
/// Runs in **stateless JSON mode**: each POST returns its result directly as
/// `application/json` (no MCP session id, no SSE channel). Our tools are
/// stateless at the protocol level — app sessions are explicit handles passed
/// as tool arguments — so this is simpler for clients and horizontally
/// scalable. Host validation is disabled because the bearer-token middleware is
/// the security boundary and the server may bind non-loopback addresses.
pub fn service(state: AppState) -> StreamableHttpService<McpServer, LocalSessionManager> {
    let services = state.services.clone();
    let config = rmcp::transport::streamable_http_server::StreamableHttpServerConfig::default()
        .with_stateful_mode(false)
        .with_json_response(true)
        .disable_allowed_hosts();
    StreamableHttpService::new(
        move || Ok(McpServer::new(services.clone())),
        Arc::new(LocalSessionManager::default()),
        config,
    )
}

// ------------------------------- tool argument structs (JSON Schema inputs) --

#[derive(Debug, Deserialize, JsonSchema)]
struct RepoArg {
    /// Repository id as configured on the server.
    repo: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchArgs {
    /// The pattern to search for.
    pattern: String,
    /// Treat `pattern` as a regular expression (default: literal).
    #[serde(default)]
    regex: bool,
    /// Case mode: "smart" (default), "sensitive", or "insensitive".
    #[serde(default)]
    case: Option<String>,
    /// Stop after this many matches.
    #[serde(default)]
    max_results: Option<usize>,
    /// Context lines before each match.
    #[serde(default)]
    context_before: Option<usize>,
    /// Context lines after each match.
    #[serde(default)]
    context_after: Option<usize>,
    /// Restrict to this subtree (relative to the view root).
    #[serde(default)]
    subtree: Option<String>,
    /// Restrict to files matching these globs.
    #[serde(default)]
    globs: Option<Vec<String>>,
    /// Search only this repository. Omit to search all.
    #[serde(default)]
    repo: Option<String>,
    /// Search within this session's overlay instead of a bare repo.
    #[serde(default)]
    session: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReadArgs {
    /// Repository id (required unless `session` is given).
    #[serde(default)]
    repo: Option<String>,
    /// Session id; when set, reads through the session overlay.
    #[serde(default)]
    session: Option<String>,
    /// File path, relative to the repo/session root.
    path: String,
    /// First line (1-based). Omit for whole file.
    #[serde(default)]
    start: Option<u32>,
    /// Last line (1-based, inclusive).
    #[serde(default)]
    end: Option<u32>,
    /// Revision (Git: branch/tag/commit; SVN: revnum). Repo reads only.
    #[serde(default)]
    rev: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct BlameArgs {
    repo: String,
    path: String,
    #[serde(default)]
    rev: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct HistoryArgs {
    repo: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    skip: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SessionArg {
    session_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WriteArgs {
    session_id: String,
    path: String,
    /// New file content (UTF-8 text).
    content: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EditArgs {
    session_id: String,
    path: String,
    /// A strict unified/git diff to apply (provide this OR `edits`, not both).
    #[serde(default)]
    diff: Option<String>,
    /// Literal search/replace blocks to apply (provide this OR `diff`).
    #[serde(default)]
    edits: Option<Vec<mortis_core::Replacement>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SessionPathArg {
    session_id: String,
    path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DiffArgs {
    session_id: String,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AsmCreateArgs {
    /// URL of the binary to download (http/https; host must be allowlisted).
    url: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AsmSessionArg {
    asm_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AsmDisasmArgs {
    asm_id: String,
    /// Start virtual address (decimal, or a `0x`-prefixed hex string).
    start: String,
    /// Number of bytes to disassemble.
    len: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AsmFnArgs {
    asm_id: String,
    /// Virtual address to resolve (decimal, or a `0x`-prefixed hex string).
    address: String,
}

// ----------------------------------------------------------------- the tools --

#[tool_router]
impl McpServer {
    #[tool(description = "List configured repositories and their sync status.")]
    async fn list_repos(&self) -> Result<String, ErrorData> {
        ok_json(self.services.list_repos())
    }

    #[tool(description = "Fetch/update a repository and re-materialize its whitelisted tree.")]
    async fn sync_repo(&self, Parameters(a): Parameters<RepoArg>) -> Result<String, ErrorData> {
        let snap = self.services.sync_repo(&RepoId::from(a.repo)).await.map_err(to_mcp_error)?;
        ok_json(snap)
    }

    #[tool(description = "Search code across repositories, one repository, or within a session overlay.")]
    async fn search_code(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(a): Parameters<SearchArgs>,
    ) -> Result<String, ErrorData> {
        let query = build_query(&a);
        let hits = if let Some(sid) = a.session {
            let principal = principal_of(&parts)?;
            self.services
                .search_session(&principal, &SessionId::from(sid.as_str()), query)
                .await
        } else if let Some(repo) = a.repo {
            self.services.search_repo(&RepoId::from(repo), query).await
        } else {
            self.services.search_all(query).await
        };
        ok_json(hits.map_err(to_mcp_error)?)
    }

    #[tool(description = "Read a file (optionally a line range) from a repo revision or a session overlay.")]
    async fn read_file(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(a): Parameters<ReadArgs>,
    ) -> Result<String, ErrorData> {
        let range = line_range(a.start, a.end);
        let content = if let Some(sid) = a.session {
            let principal = principal_of(&parts)?;
            self.services
                .read_session_file(&principal, &SessionId::from(sid.as_str()), Utf8Path::new(&a.path), range)
                .await
        } else {
            let repo = a.repo.ok_or_else(|| ErrorData::invalid_params("either repo or session is required", None))?;
            let rev = Rev::from_opt(a.rev);
            self.services
                .read_repo_file(&RepoId::from(repo), Utf8Path::new(&a.path), &rev, range)
                .await
        };
        ok_json(content.map_err(to_mcp_error)?)
    }

    #[tool(description = "Blame a file against the original repository.")]
    async fn blame_file(&self, Parameters(a): Parameters<BlameArgs>) -> Result<String, ErrorData> {
        let rev = Rev::from_opt(a.rev);
        let lines = self
            .services
            .blame(&RepoId::from(a.repo), Utf8Path::new(&a.path), &rev)
            .await
            .map_err(to_mcp_error)?;
        ok_json(lines)
    }

    #[tool(description = "Commit history for a repository or a single file.")]
    async fn get_history(&self, Parameters(a): Parameters<HistoryArgs>) -> Result<String, ErrorData> {
        let query = LogQuery { max_count: a.limit, skip: a.skip };
        let path = a.path.as_deref().map(Utf8Path::new);
        let commits = self
            .services
            .history(&RepoId::from(a.repo), path, &query)
            .await
            .map_err(to_mcp_error)?;
        ok_json(commits)
    }

    #[tool(description = "Create a copy-on-write session over a repository's current head.")]
    async fn create_session(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(a): Parameters<RepoArg>,
    ) -> Result<String, ErrorData> {
        let principal = principal_of(&parts)?;
        let s = self.services.create_session(&principal, &RepoId::from(a.repo)).await.map_err(to_mcp_error)?;
        ok_json(s)
    }

    #[tool(description = "List the caller's sessions.")]
    async fn list_sessions(&self, Extension(parts): Extension<Parts>) -> Result<String, ErrorData> {
        let principal = principal_of(&parts)?;
        let list = self.services.list_sessions(&principal).await.map_err(to_mcp_error)?;
        ok_json(list)
    }

    #[tool(description = "Delete one of the caller's sessions.")]
    async fn delete_session(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(a): Parameters<SessionArg>,
    ) -> Result<String, ErrorData> {
        let principal = principal_of(&parts)?;
        self.services.delete_session(&principal, &SessionId::from(a.session_id.as_str())).await.map_err(to_mcp_error)?;
        ok_json(serde_json::json!({ "deleted": a.session_id }))
    }

    #[tool(description = "Write (create or overwrite) a file in a session's CoW layer.")]
    async fn write_file(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(a): Parameters<WriteArgs>,
    ) -> Result<String, ErrorData> {
        let principal = principal_of(&parts)?;
        self.services
            .write_file(&principal, &SessionId::from(a.session_id.as_str()), Utf8Path::new(&a.path), a.content.as_bytes())
            .await
            .map_err(to_mcp_error)?;
        ok_json(serde_json::json!({ "written": a.path, "bytes": a.content.len() }))
    }

    #[tool(
        description = "Edit a file in a session via a strict unified diff OR literal search/replace blocks (provide exactly one of `diff`/`edits`). Fails if the diff does not apply cleanly or a search is not uniquely found."
    )]
    async fn edit_file(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(a): Parameters<EditArgs>,
    ) -> Result<String, ErrorData> {
        let principal = principal_of(&parts)?;
        let edit = match (a.diff, a.edits) {
            (Some(d), None) => mortis_core::FileEdit::UnifiedDiff(d),
            (None, Some(e)) => mortis_core::FileEdit::SearchReplace(e),
            _ => {
                return Err(ErrorData::invalid_params(
                    "provide exactly one of `diff` or `edits`",
                    None,
                ));
            }
        };
        let outcome = self
            .services
            .edit_file(&principal, &SessionId::from(a.session_id.as_str()), Utf8Path::new(&a.path), edit)
            .await
            .map_err(to_mcp_error)?;
        ok_json(outcome)
    }

    #[tool(description = "Delete a file in a session view (whiteout if it exists in the base).")]
    async fn delete_file(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(a): Parameters<SessionPathArg>,
    ) -> Result<String, ErrorData> {
        let principal = principal_of(&parts)?;
        self.services
            .delete_file(&principal, &SessionId::from(a.session_id.as_str()), Utf8Path::new(&a.path))
            .await
            .map_err(to_mcp_error)?;
        ok_json(serde_json::json!({ "deleted": a.path }))
    }

    #[tool(description = "Git-style status of a session (added/modified/deleted).")]
    async fn session_status(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(a): Parameters<SessionArg>,
    ) -> Result<String, ErrorData> {
        let principal = principal_of(&parts)?;
        let st = self.services.session_status(&principal, &SessionId::from(a.session_id.as_str())).await.map_err(to_mcp_error)?;
        ok_json(st)
    }

    #[tool(description = "Unified diff for one file or the whole session.")]
    async fn session_diff(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(a): Parameters<DiffArgs>,
    ) -> Result<String, ErrorData> {
        let principal = principal_of(&parts)?;
        let path = a.path.as_deref().map(Utf8Path::new);
        let diff = self.services.session_diff(&principal, &SessionId::from(a.session_id.as_str()), path).await.map_err(to_mcp_error)?;
        ok_json(serde_json::json!({ "diff": diff }))
    }

    #[tool(description = "Export a session's full change set as a git-apply-able patch.")]
    async fn export_patch(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(a): Parameters<SessionArg>,
    ) -> Result<String, ErrorData> {
        let principal = principal_of(&parts)?;
        let patch = self.services.export_patch(&principal, &SessionId::from(a.session_id.as_str())).await.map_err(to_mcp_error)?;
        ok_json(serde_json::json!({ "patch": patch }))
    }

    #[tool(
        description = "Create an assembly-query session: download a binary from a URL (http/https, host must be allowlisted) and validate it in the background. Poll get_asm_session for status/progress."
    )]
    async fn create_asm_session(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(a): Parameters<AsmCreateArgs>,
    ) -> Result<String, ErrorData> {
        let principal = principal_of(&parts)?;
        let s = self.services.create_asm_session(&principal, &a.url).await.map_err(to_mcp_error)?;
        ok_json(s)
    }

    #[tool(description = "List the caller's assembly-query sessions.")]
    async fn list_asm_sessions(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<String, ErrorData> {
        let principal = principal_of(&parts)?;
        let list = self.services.list_asm_sessions(&principal).await.map_err(to_mcp_error)?;
        ok_json(list)
    }

    #[tool(
        description = "Get an assembly session's status: download progress, the validated binary's OS/format/arch metadata when ready, or the failure reason."
    )]
    async fn get_asm_session(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(a): Parameters<AsmSessionArg>,
    ) -> Result<String, ErrorData> {
        let principal = principal_of(&parts)?;
        let s = self.services.get_asm_session(&principal, &AsmSessionId::from(a.asm_id.as_str())).await.map_err(to_mcp_error)?;
        ok_json(s)
    }

    #[tool(description = "Delete one of the caller's assembly sessions and its downloaded binary.")]
    async fn delete_asm_session(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(a): Parameters<AsmSessionArg>,
    ) -> Result<String, ErrorData> {
        let principal = principal_of(&parts)?;
        self.services.delete_asm_session(&principal, &AsmSessionId::from(a.asm_id.as_str())).await.map_err(to_mcp_error)?;
        ok_json(serde_json::json!({ "deleted": a.asm_id }))
    }

    #[tool(description = "Disassemble a virtual-address range of a ready assembly-session binary.")]
    async fn asm_disassemble(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(a): Parameters<AsmDisasmArgs>,
    ) -> Result<String, ErrorData> {
        let principal = principal_of(&parts)?;
        let start = parse_u64_arg(&a.start)?;
        let dis = self
            .services
            .asm_disassemble(&principal, &AsmSessionId::from(a.asm_id.as_str()), start, a.len)
            .await
            .map_err(to_mcp_error)?;
        ok_json(dis)
    }

    #[tool(description = "Resolve a virtual address to a function name in a ready assembly-session binary.")]
    async fn asm_resolve_function(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(a): Parameters<AsmFnArgs>,
    ) -> Result<String, ErrorData> {
        let principal = principal_of(&parts)?;
        let address = parse_u64_arg(&a.address)?;
        let r = self
            .services
            .asm_resolve_function(&principal, &AsmSessionId::from(a.asm_id.as_str()), address)
            .await
            .map_err(to_mcp_error)?;
        ok_json(r)
    }

    #[tool(description = "Read header/section metadata of a ready assembly-session binary.")]
    async fn asm_metadata(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(a): Parameters<AsmSessionArg>,
    ) -> Result<String, ErrorData> {
        let principal = principal_of(&parts)?;
        let meta = self.services.asm_metadata(&principal, &AsmSessionId::from(a.asm_id.as_str())).await.map_err(to_mcp_error)?;
        ok_json(meta)
    }
}

#[tool_handler]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo is #[non_exhaustive]; start from default and set fields.
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info.name = "mortis-code-server".to_string();
        info.server_info.version = env!("CARGO_PKG_VERSION").to_string();
        info.instructions = Some(
            "mortis-code-server: search, read, blame, and history over Git/SVN repos, \
             plus copy-on-write sessions for edits (write_file/edit_file via diff or \
             search/replace, status/diff/patch), and assembly-query sessions that download \
             a binary and disassemble it / resolve addresses / read metadata. All tools \
             require a bearer token; session tools are scoped to the caller."
                .to_string(),
        );
        info
    }
}

// ----------------------------------------------------------------- helpers ----

/// Recover the authenticated principal injected by the auth middleware.
fn principal_of(parts: &Parts) -> Result<Principal, ErrorData> {
    parts
        .extensions
        .get::<Principal>()
        .cloned()
        .ok_or_else(|| ErrorData::invalid_request("unauthenticated request", None))
}

/// Serialize any value into a JSON text tool result.
///
/// We return JSON as text content (not rmcp's structured `Json<T>`) because
/// rmcp validates structured output schemas to have a root `type: object`,
/// which our heterogeneous results (arrays, etc.) do not satisfy.
fn ok_json<T: serde::Serialize>(value: T) -> Result<String, ErrorData> {
    serde_json::to_string(&value)
        .map_err(|e| ErrorData::internal_error(format!("serialization failed: {e}"), None))
}

/// Parse a `u64` tool argument from a decimal or `0x`-prefixed hex string.
fn parse_u64_arg(s: &str) -> Result<u64, ErrorData> {
    let t = s.trim();
    let parsed = if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16)
    } else {
        t.parse::<u64>()
    };
    parsed.map_err(|_| ErrorData::invalid_params(format!("invalid number: {s:?}"), None))
}

fn case_mode(s: Option<&str>) -> CaseMode {
    match s {
        Some("sensitive") => CaseMode::Sensitive,
        Some("insensitive") => CaseMode::Insensitive,
        _ => CaseMode::Smart,
    }
}

fn build_query(a: &SearchArgs) -> SearchQuery {
    SearchQuery {
        pattern: a.pattern.clone(),
        regex: a.regex,
        case: case_mode(a.case.as_deref()),
        max_results: a.max_results,
        context_before: a.context_before.unwrap_or(0),
        context_after: a.context_after.unwrap_or(0),
        subtree: a.subtree.as_deref().map(Utf8PathBuf::from),
        globs: a.globs.clone().unwrap_or_default(),
    }
}


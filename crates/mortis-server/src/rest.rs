//! REST/JSON adapter. Every handler is a thin shim over [`Services`]; the MCP
//! adapter calls the very same methods, keeping the two protocols equivalent.

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Extension, Path, Query, State},
    routing::{get, post, put},
};
use camino::Utf8Path;
use serde::Deserialize;

use mortis_core::{
    AsmSessionId, CoreError, LogQuery, Principal, RepoId, Rev, SearchQuery, SessionId, line_range,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// Build the protected `/api/v1` router.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/repos", get(list_repos))
        .route("/api/v1/repos/{id}/sync", post(sync_repo))
        .route("/api/v1/repos/{id}/file", get(read_repo_file))
        .route("/api/v1/repos/{id}/blame", get(blame))
        .route("/api/v1/repos/{id}/history", get(history))
        .route("/api/v1/search", post(search))
        .route("/api/v1/sessions", post(create_session).get(list_sessions))
        .route(
            "/api/v1/sessions/{sid}",
            get(get_session).delete(delete_session),
        )
        .route(
            "/api/v1/sessions/{sid}/file",
            put(write_file)
                .patch(edit_file)
                .delete(delete_file)
                .get(read_session_file),
        )
        .route("/api/v1/sessions/{sid}/status", get(session_status))
        .route("/api/v1/sessions/{sid}/diff", get(session_diff))
        .route("/api/v1/sessions/{sid}/patch", get(export_patch))
        // assembly-query sessions
        .route("/api/v1/asm/sessions", post(create_asm).get(list_asm))
        .route(
            "/api/v1/asm/sessions/{aid}",
            get(get_asm).delete(delete_asm),
        )
        .route("/api/v1/asm/sessions/{aid}/disasm", get(asm_disasm))
        .route("/api/v1/asm/sessions/{aid}/function", get(asm_function))
        .route("/api/v1/asm/sessions/{aid}/metadata", get(asm_metadata))
}

// --------------------------------------------------------------------- repos

async fn list_repos(State(st): State<AppState>) -> Json<Vec<mortis_app::RepoInfo>> {
    Json(st.services.list_repos())
}

async fn sync_repo(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<mortis_core::RepoSnapshot>> {
    Ok(Json(st.services.sync_repo(&RepoId::from(id)).await?))
}

#[derive(Debug, Deserialize)]
struct ReadQuery {
    path: String,
    #[serde(default)]
    start: Option<u32>,
    #[serde(default)]
    end: Option<u32>,
    #[serde(default)]
    rev: Option<String>,
}

async fn read_repo_file(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ReadQuery>,
) -> ApiResult<Json<mortis_core::FileContent>> {
    let rev = Rev::from_opt(q.rev);
    let content = st
        .services
        .read_repo_file(&RepoId::from(id), Utf8Path::new(&q.path), &rev, line_range(q.start, q.end))
        .await?;
    Ok(Json(content))
}

#[derive(Debug, Deserialize)]
struct BlameQuery {
    path: String,
    #[serde(default)]
    rev: Option<String>,
}

async fn blame(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<BlameQuery>,
) -> ApiResult<Json<Vec<mortis_core::BlameLine>>> {
    let rev = Rev::from_opt(q.rev);
    Ok(Json(
        st.services.blame(&RepoId::from(id), Utf8Path::new(&q.path), &rev).await?,
    ))
}

#[derive(Debug, Deserialize)]
struct HistoryQuery {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    skip: Option<usize>,
}

async fn history(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> ApiResult<Json<Vec<mortis_core::Commit>>> {
    let query = LogQuery {
        max_count: q.limit,
        skip: q.skip,
    };
    let path = q.path.as_deref().map(Utf8Path::new);
    Ok(Json(st.services.history(&RepoId::from(id), path, &query).await?))
}

// -------------------------------------------------------------------- search

#[derive(Debug, Deserialize)]
struct SearchRequest {
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    session: Option<String>,
    #[serde(flatten)]
    query: SearchQuery,
}

async fn search(
    State(st): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(req): Json<SearchRequest>,
) -> ApiResult<Json<Vec<mortis_core::SearchMatch>>> {
    let hits = if let Some(sid) = req.session {
        st.services
            .search_session(&principal, &SessionId::from(sid.as_str()), req.query)
            .await?
    } else if let Some(repo) = req.repo {
        st.services.search_repo(&RepoId::from(repo), req.query).await?
    } else {
        st.services.search_all(req.query).await?
    };
    Ok(Json(hits))
}

// ------------------------------------------------------------------ sessions

#[derive(Debug, Deserialize)]
struct CreateSessionRequest {
    repo: String,
}

async fn create_session(
    State(st): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(req): Json<CreateSessionRequest>,
) -> ApiResult<Json<mortis_core::Session>> {
    Ok(Json(
        st.services.create_session(&principal, &RepoId::from(req.repo)).await?,
    ))
}

async fn list_sessions(
    State(st): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<Vec<mortis_core::Session>>> {
    Ok(Json(st.services.list_sessions(&principal).await?))
}

async fn get_session(
    State(st): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(sid): Path<String>,
) -> ApiResult<Json<mortis_core::Session>> {
    Ok(Json(
        st.services.get_session(&principal, &SessionId::from(sid.as_str())).await?,
    ))
}

async fn delete_session(
    State(st): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(sid): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    st.services.delete_session(&principal, &SessionId::from(sid.as_str())).await?;
    Ok(Json(serde_json::json!({ "deleted": sid })))
}

#[derive(Debug, Deserialize)]
struct PathQuery {
    path: String,
}

async fn write_file(
    State(st): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(sid): Path<String>,
    Query(q): Query<PathQuery>,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    st.services
        .write_file(&principal, &SessionId::from(sid.as_str()), Utf8Path::new(&q.path), &body)
        .await?;
    Ok(Json(serde_json::json!({ "written": q.path, "bytes": body.len() })))
}

async fn delete_file(
    State(st): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(sid): Path<String>,
    Query(q): Query<PathQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    st.services
        .delete_file(&principal, &SessionId::from(sid.as_str()), Utf8Path::new(&q.path))
        .await?;
    Ok(Json(serde_json::json!({ "deleted": q.path })))
}

/// Body for `PATCH /sessions/{sid}/file`: exactly one of `diff` or `edits`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditRequest {
    /// A strict unified/git diff to apply to the target file.
    #[serde(default)]
    diff: Option<String>,
    /// Literal search/replace blocks to apply to the target file.
    #[serde(default)]
    edits: Option<Vec<mortis_core::Replacement>>,
}

async fn edit_file(
    State(st): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(sid): Path<String>,
    Query(q): Query<PathQuery>,
    Json(req): Json<EditRequest>,
) -> ApiResult<Json<mortis_core::EditOutcome>> {
    let edit = match (req.diff, req.edits) {
        (Some(d), None) => mortis_core::FileEdit::UnifiedDiff(d),
        (None, Some(e)) => mortis_core::FileEdit::SearchReplace(e),
        _ => {
            return Err(mortis_core::CoreError::invalid(
                "provide exactly one of `diff` or `edits`",
            )
            .into());
        }
    };
    let outcome = st
        .services
        .edit_file(&principal, &SessionId::from(sid.as_str()), Utf8Path::new(&q.path), edit)
        .await?;
    Ok(Json(outcome))
}

async fn read_session_file(
    State(st): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(sid): Path<String>,
    Query(q): Query<ReadQuery>,
) -> ApiResult<Json<mortis_core::FileContent>> {
    let content = st
        .services
        .read_session_file(
            &principal,
            &SessionId::from(sid.as_str()),
            Utf8Path::new(&q.path),
            line_range(q.start, q.end),
        )
        .await?;
    Ok(Json(content))
}

async fn session_status(
    State(st): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(sid): Path<String>,
) -> ApiResult<Json<Vec<mortis_core::FileStatus>>> {
    Ok(Json(
        st.services.session_status(&principal, &SessionId::from(sid.as_str())).await?,
    ))
}

#[derive(Debug, Deserialize)]
struct DiffQuery {
    #[serde(default)]
    path: Option<String>,
}

async fn session_diff(
    State(st): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(sid): Path<String>,
    Query(q): Query<DiffQuery>,
) -> ApiResult<String> {
    let path = q.path.as_deref().map(Utf8Path::new);
    Ok(st.services.session_diff(&principal, &SessionId::from(sid.as_str()), path).await?)
}

async fn export_patch(
    State(st): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(sid): Path<String>,
) -> ApiResult<String> {
    Ok(st.services.export_patch(&principal, &SessionId::from(sid.as_str())).await?)
}

// ----------------------------------------------------------- assembly sessions

/// Parse a `u64` from a decimal or `0x`-prefixed hex string.
fn parse_u64(s: &str) -> Result<u64, CoreError> {
    let t = s.trim();
    let parsed = if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16)
    } else {
        t.parse::<u64>()
    };
    parsed.map_err(|_| CoreError::invalid(format!("invalid number: {s:?}")))
}

#[derive(Debug, Deserialize)]
struct CreateAsmRequest {
    url: String,
}

async fn create_asm(
    State(st): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(req): Json<CreateAsmRequest>,
) -> ApiResult<Json<mortis_core::AsmSession>> {
    Ok(Json(
        st.services.create_asm_session(&principal, &req.url).await?,
    ))
}

async fn list_asm(
    State(st): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<Vec<mortis_core::AsmSession>>> {
    Ok(Json(st.services.list_asm_sessions(&principal).await?))
}

async fn get_asm(
    State(st): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(aid): Path<String>,
) -> ApiResult<Json<mortis_core::AsmSession>> {
    Ok(Json(
        st.services.get_asm_session(&principal, &AsmSessionId::from(aid.as_str())).await?,
    ))
}

async fn delete_asm(
    State(st): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(aid): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    st.services.delete_asm_session(&principal, &AsmSessionId::from(aid.as_str())).await?;
    Ok(Json(serde_json::json!({ "deleted": aid })))
}

#[derive(Debug, Deserialize)]
struct DisasmQuery {
    /// Start virtual address (decimal or `0x`-hex).
    start: String,
    /// Number of bytes to disassemble.
    len: u64,
}

async fn asm_disasm(
    State(st): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(aid): Path<String>,
    Query(q): Query<DisasmQuery>,
) -> ApiResult<Json<mortis_core::Disassembly>> {
    let start = parse_u64(&q.start)?;
    Ok(Json(
        st.services
            .asm_disassemble(&principal, &AsmSessionId::from(aid.as_str()), start, q.len)
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
struct FnQuery {
    /// Virtual address to resolve (decimal or `0x`-hex).
    address: String,
}

async fn asm_function(
    State(st): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(aid): Path<String>,
    Query(q): Query<FnQuery>,
) -> ApiResult<Json<mortis_core::FunctionResolution>> {
    let address = parse_u64(&q.address)?;
    Ok(Json(
        st.services
            .asm_resolve_function(&principal, &AsmSessionId::from(aid.as_str()), address)
            .await?,
    ))
}

async fn asm_metadata(
    State(st): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(aid): Path<String>,
) -> ApiResult<Json<mortis_core::BinaryInfo>> {
    Ok(Json(
        st.services.asm_metadata(&principal, &AsmSessionId::from(aid.as_str())).await?,
    ))
}

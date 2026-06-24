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
    LogQuery, Principal, RepoId, Rev, SearchQuery, SessionId, line_range,
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
            put(write_file).delete(delete_file).get(read_session_file),
        )
        .route("/api/v1/sessions/{sid}/status", get(session_status))
        .route("/api/v1/sessions/{sid}/diff", get(session_diff))
        .route("/api/v1/sessions/{sid}/patch", get(export_patch))
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

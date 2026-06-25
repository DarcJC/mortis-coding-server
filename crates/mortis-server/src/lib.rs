//! # mortis-server
//!
//! Wires the domain services into an `axum` application that serves both a
//! REST/JSON API (`/api/v1`) and a Streamable-HTTP MCP endpoint (`/mcp`) behind
//! bearer-token auth, and runs scheduled syncs + a session reaper.
//!
//! The binary (`mortis-code-server`) is a thin wrapper over [`run`]. Tests build
//! the app via [`build_services`] + [`build_app`] and drive it with `oneshot`.

pub mod auth;
pub mod config;
pub mod error;
pub mod logging;
pub mod mcp;
pub mod rest;
pub mod scheduler;
pub mod state;

use std::sync::Arc;

use axum::{Router, middleware::from_fn_with_state, routing::get};

use mortis_app::{BackendSet, RepoRegistry, Services};
use mortis_asm::MemAssemblyStore;
use mortis_search::GrepSearchEngine;
use mortis_session::DiskSessionStore;
use mortis_vcs::GixBackend;

use crate::auth::{Authenticator, require_bearer};
use crate::config::Config;
use crate::state::AppState;

/// Construct the dependency-injected services and shared state from config.
///
/// This is the composition root: it picks concrete backends (`GixBackend`,
/// `GrepSearchEngine`, `DiskSessionStore`) and injects them through the
/// `mortis-app` traits.
pub fn build_services(config: Config) -> anyhow::Result<(AppState, Arc<Services>)> {
    let data_dir = config.server.data_dir.clone();
    std::fs::create_dir_all(&data_dir)?;
    let sessions_dir = data_dir.join("sessions");

    let git: Arc<dyn mortis_core::VcsBackend> = Arc::new(GixBackend::new());

    // SVN is best-effort: resolve an svn tool (embedded → system, or override).
    // If none is available it simply isn't offered; configuring an SVN repo
    // without a tool then fails fast at registry build with a clear message.
    let cache_dir = data_dir.join("cache");
    let svn: Option<Arc<dyn mortis_core::VcsBackend>> =
        match mortis_vcs::SvnCliBackend::resolve(&cache_dir, config.server.svn_bin.as_deref()) {
            Ok(backend) => {
                tracing::info!("svn backend ready (source: {:?})", backend.source());
                Some(Arc::new(backend))
            }
            Err(e) => {
                tracing::warn!("svn backend unavailable: {e}");
                None
            }
        };

    let backends = BackendSet { git, svn };
    let registry = Arc::new(RepoRegistry::build(config.repos, &data_dir, &backends)?);
    let search = Arc::new(GrepSearchEngine::new());
    let sessions = Arc::new(DiskSessionStore::new(sessions_dir)?);

    // Assembly-query sessions: downloaded binaries live under `<data>/asm`.
    let asm_dir = config
        .asm
        .download_dir
        .clone()
        .unwrap_or_else(|| data_dir.join("asm"));
    let asm = Arc::new(MemAssemblyStore::new(asm_dir, config.asm.policy())?);

    let services = Arc::new(Services::new(registry, search, sessions, asm));

    let auth = Arc::new(Authenticator::new(&config.auth.tokens));
    let state = AppState {
        services: services.clone(),
        auth,
    };
    Ok((state, services))
}

/// Assemble the HTTP router: REST + MCP behind bearer auth; `/health` public.
pub fn build_app(state: AppState) -> Router {
    Router::new()
        .merge(rest::router())
        .route_service("/mcp", mcp::service(state.clone()))
        .layer(from_fn_with_state(state.clone(), require_bearer))
        .route("/health", get(|| async { "ok" }))
        .with_state(state)
}

/// Full server lifecycle: build services, kick off an initial sync, start the
/// scheduler, and serve until the process is stopped.
pub async fn run(config: Config) -> anyhow::Result<()> {
    use anyhow::Context;

    // Initialize logging before anything else so all startup output is captured.
    // Hold the guard (if a file sink is configured) for the whole server life.
    let _log_guard = logging::init(
        config.server.log_file.as_deref(),
        config.server.log_level.as_deref(),
    )?;

    let bind = config.server.bind.clone();
    let ttl = config.session.ttl_duration();
    let reap = config.session.reap_duration();
    let asm_ttl = config.asm.ttl_duration();

    let (state, services) = build_services(config)?;
    if state.auth.is_empty() {
        tracing::warn!("no [auth] tokens configured — every request will be rejected with 401");
    }

    {
        let services = services.clone();
        tokio::spawn(async move {
            for (id, res) in services.sync_all().await {
                match res {
                    Ok(snap) => tracing::info!("initial sync '{id}' @ {}", snap.head),
                    Err(e) => tracing::warn!("initial sync '{id}' failed: {e}"),
                }
            }
        });
    }

    let _scheduler = scheduler::start(services.clone(), ttl, reap, asm_ttl)
        .await
        .context("starting scheduler")?;

    let app = build_app(state);
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    tracing::info!("mortis-code-server listening on http://{bind} (REST: /api/v1, MCP: /mcp)");
    axum::serve(listener, app).await.context("server error")?;
    Ok(())
}

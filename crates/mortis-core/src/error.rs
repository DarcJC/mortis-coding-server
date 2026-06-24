//! Unified error type shared across all layers.
//!
//! Infrastructure crates map their library-specific errors into [`CoreError`];
//! the presentation layer maps [`CoreError`] back out to HTTP status codes and
//! MCP error objects. Keeping a single error vocabulary is what lets the REST
//! and MCP adapters stay thin.

use thiserror::Error;

/// Convenience alias used throughout the workspace.
pub type Result<T> = std::result::Result<T, CoreError>;

/// The canonical error type for the domain.
#[derive(Debug, Error)]
pub enum CoreError {
    /// A requested entity (repo, session, file, revision) does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// The caller supplied invalid arguments.
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// Authentication failed or was missing.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// The caller is authenticated but not allowed to touch this resource.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// The operation conflicts with current state.
    #[error("conflict: {0}")]
    Conflict(String),

    /// A version-control backend failed.
    #[error("vcs error: {0}")]
    Vcs(String),

    /// The search engine failed.
    #[error("search error: {0}")]
    Search(String),

    /// The session store failed.
    #[error("session error: {0}")]
    Session(String),

    /// Configuration is malformed.
    #[error("config error: {0}")]
    Config(String),

    /// Underlying I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Anything that does not fit the buckets above.
    #[error("{0}")]
    Other(String),
}

impl CoreError {
    /// A short, stable, machine-readable code for this error category.
    ///
    /// Used by the MCP/REST adapters to produce consistent error payloads.
    pub fn code(&self) -> &'static str {
        match self {
            CoreError::NotFound(_) => "not_found",
            CoreError::InvalidInput(_) => "invalid_input",
            CoreError::Unauthorized(_) => "unauthorized",
            CoreError::Forbidden(_) => "forbidden",
            CoreError::Conflict(_) => "conflict",
            CoreError::Vcs(_) => "vcs_error",
            CoreError::Search(_) => "search_error",
            CoreError::Session(_) => "session_error",
            CoreError::Config(_) => "config_error",
            CoreError::Io(_) => "io_error",
            CoreError::Other(_) => "internal_error",
        }
    }

    /// Helper for the common "not found" case.
    pub fn not_found(what: impl Into<String>) -> Self {
        CoreError::NotFound(what.into())
    }

    /// Helper for the common "invalid input" case.
    pub fn invalid(what: impl Into<String>) -> Self {
        CoreError::InvalidInput(what.into())
    }
}

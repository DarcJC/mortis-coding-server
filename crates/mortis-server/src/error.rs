//! Error mapping from the domain [`CoreError`] to HTTP responses and MCP errors.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

use mortis_core::CoreError;

/// Newtype wrapping a [`CoreError`] so it can implement axum's `IntoResponse`.
pub struct ApiError(pub CoreError);

impl From<CoreError> for ApiError {
    fn from(e: CoreError) -> Self {
        ApiError(e)
    }
}

/// Convenience result alias for REST handlers.
pub type ApiResult<T> = Result<T, ApiError>;

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            CoreError::NotFound(_) => StatusCode::NOT_FOUND,
            CoreError::InvalidInput(_) => StatusCode::BAD_REQUEST,
            CoreError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            CoreError::Forbidden(_) => StatusCode::FORBIDDEN,
            CoreError::Conflict(_) => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = Json(json!({
            "error": self.0.code(),
            "message": self.0.to_string(),
        }));
        (status, body).into_response()
    }
}

/// Map a [`CoreError`] to an MCP error object.
pub fn to_mcp_error(e: CoreError) -> rmcp::ErrorData {
    use rmcp::ErrorData;
    match &e {
        CoreError::InvalidInput(_) | CoreError::NotFound(_) | CoreError::Forbidden(_) => {
            ErrorData::invalid_params(e.to_string(), None)
        }
        _ => ErrorData::internal_error(e.to_string(), None),
    }
}

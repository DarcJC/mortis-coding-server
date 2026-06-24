//! Bearer-token authentication.
//!
//! A single tower middleware guards both the REST and MCP routes: it validates
//! the `Authorization: Bearer <token>` header in constant time and, on success,
//! injects the resolved [`Principal`] into the request extensions. REST handlers
//! read it via `Extension<Principal>`; MCP tools read it from the captured
//! `http::request::Parts` extensions.

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode, header},
    middleware::Next,
    response::Response,
};
use subtle::ConstantTimeEq;

use mortis_core::Principal;

use crate::config::TokenEntry;
use crate::state::AppState;

/// Resolves bearer tokens to principals.
#[derive(Debug, Default)]
pub struct Authenticator {
    tokens: Vec<(String, Principal)>,
}

impl Authenticator {
    pub fn new(entries: &[TokenEntry]) -> Self {
        Self {
            tokens: entries
                .iter()
                .map(|e| (e.token.clone(), Principal::from(e.principal.as_str())))
                .collect(),
        }
    }

    /// Return the principal for `presented`, comparing every token in constant
    /// time to avoid leaking which (if any) token prefix matched.
    pub fn authenticate(&self, presented: &str) -> Option<Principal> {
        let mut found: Option<Principal> = None;
        for (token, principal) in &self.tokens {
            if bool::from(token.as_bytes().ct_eq(presented.as_bytes())) {
                found = Some(principal.clone());
            }
        }
        found
    }

    /// Whether any tokens are configured.
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }
}

/// Axum middleware enforcing bearer auth and injecting the [`Principal`].
pub async fn require_bearer(
    State(state): State<AppState>,
    mut req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::trim);

    match presented.and_then(|t| state.auth.authenticate(t)) {
        Some(principal) => {
            req.extensions_mut().insert(principal);
            Ok(next.run(req).await)
        }
        None => Err(StatusCode::UNAUTHORIZED),
    }
}

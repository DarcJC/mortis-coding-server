//! Shared application state handed to every request handler.

use std::sync::Arc;

use mortis_app::Services;

use crate::auth::Authenticator;

/// Cheaply-cloneable shared state (everything behind `Arc`).
#[derive(Clone)]
pub struct AppState {
    pub services: Arc<Services>,
    pub auth: Arc<Authenticator>,
}

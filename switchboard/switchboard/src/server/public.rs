//! Bootstrapping for the public API server.

use crate::server::{session, AppState};
use axum::routing::post;
use axum::Router;

/// Sub-router for API endpoints.
pub fn build_api_router() -> Router<AppState> {
    Router::new()
}

/// Sub-router for session endpoints.
pub fn build_session_router() -> Router<AppState> {
    Router::new()
        // Standard login endpoint.
        .route("/login", post(session::login_handler))
    // TODO: other session management (logout)
}

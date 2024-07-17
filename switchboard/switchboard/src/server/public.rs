//! Bootstrapping for the public API server.

use crate::perms;
use crate::server::{session, AppState};
use axum::routing::{delete, get, post};
use axum::Router;

/// Sub-router for API endpoints.
pub fn build_api_router() -> Router<AppState> {
    Router::new()
        // job management group
        .route(
            "/job/queue",
            post(perms::jobs::enqueue).get(perms::jobs::get_queue),
        )
        .route("/job/:id", delete(perms::jobs::cancel))
        .route("/job/:id/info", get(perms::jobs::info))
        .route("/job/:id/status", get(perms::jobs::status))
}

/// Sub-router for session endpoints.
pub fn build_session_router() -> Router<AppState> {
    Router::new()
        // Standard login endpoint.
        .route("/login", post(session::login_handler))
    // TODO: other session management (logout)
}

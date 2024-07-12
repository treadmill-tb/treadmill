//! Bootstrapping for the public API server.

use crate::server::AppState;
use axum::routing::post;
use axum::Router;

/// Sub-router for API endpoints.
pub fn build_api_router() -> Router<AppState> {
    Router::new()
}

/// Sub-router for session endpoints.
pub fn build_session_router() -> Router<AppState> {
    // TODO: extract Request/Response schemas
    Router::new()
        // Creates a presession cookie/CSRF token pair that is then used to prevent SICSRF
        // (Session Initialization Cross-Site Request Forgery) attacks on `/login`.
        .route("/presession", post(super::session::presession_handler))
        // Standard login endpoint.
        .route("/login", post(super::session::login_handler))
    // TODO: other session management (logout)
}

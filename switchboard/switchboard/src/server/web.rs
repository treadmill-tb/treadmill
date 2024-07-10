use crate::server::AppState;
use axum::routing::post;
use axum::Router;

pub fn build_api_router() -> Router<AppState> {
    Router::new()
}

pub fn build_session_router() -> Router<AppState> {
    Router::new()
        .route("/presession", post(super::session::presession_handler))
        .route("/login", post(super::session::login_handler))
}

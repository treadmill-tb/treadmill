mod jobs;
mod proxy;
mod supervisors;
mod tokens;

use crate::serve::AppState;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::Router;
use http::StatusCode;
use tower_http::trace::TraceLayer;

pub fn build_router(state: AppState) -> Router<()> {
    Router::new()
        // -- INSERT ROUTES HERE --
        .nest("/api/v1", api_router())
        // utility
        .fallback(not_found)
        .with_state(state)
        .layer(TraceLayer::new_for_http())
}

fn api_router() -> Router<AppState> {
    Router::new()
        // job management group
        //  POST /jobs/new
        .route("/jobs/new", post(jobs::submit))
        //  GET /jobs (+ <FILTERS>)
        //  GET /jobs/:id/status
        .route("/jobs/:id/status", get(jobs::status))
        //  GET /jobs/:id/info
        //  DELETE /jobs/:id
        .route("/jobs/:id", delete(jobs::stop))
        // supervisor management group
        //  GET /supervisors (+ <FILTERS>)
        .route("/supervisors", get(supervisors::list))
        //  GET /supervisors/:id/status
        .route("/supervisors/:id/status", get(supervisors::status))
        //  GET /supervisors/:id/current-job
        //  DELETE /supervisors/:id/current-job
        //  POST /supervisors/new
        //  DELETE /supervisors/:id
        //  GET /supervisors/:id/connect
        // Note that the HTTP verb 'GET' here is not necessarily conformant with REST principles,
        // but is required by RFC6455 §4.1: "The method of the request MUST be GET" (regarding
        // WebSocket HTTP handshakes).
        .route("/supervisors/:id/connect", get(supervisors::connect))
        // user management group
        //  GET /users/:id/jobs (+ <FILTERS>)
        //  GET /users/:id/tokens (+ <FILTERS>)
        // token management group
        //  POST /tokens/new
        //  DELETE /tokens/:id
        //  POST /tokens/login
        // This is the only route which does not require an `Authorization: Bearer <token>` header.
        .route("/tokens/login", post(tokens::login))
}

async fn not_found() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "no such route")
}

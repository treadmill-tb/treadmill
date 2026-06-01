mod auth;
mod hosts;
mod jobs;
mod users;

use crate::serve::AppState;
use aide::axum::ApiRouter;
use aide::axum::routing::get_with;
use axum::Router;
use axum::response::IntoResponse;
use axum::routing::get;
use http::StatusCode;
use tower_http::trace::TraceLayer;

pub fn build_router(state: AppState) -> Router<()> {
    ApiRouter::new()
        // -- INSERT ROUTES HERE --
        .nest_api_service("/api/v1", api_router().with_state(state.clone()))
        // utility
        .fallback(not_found)
        .with_state(state)
        .layer(TraceLayer::new_for_http())
        .into()
}

pub fn api_router() -> ApiRouter<AppState> {
    ApiRouter::new()
        // OAuth login group (plain routes: browser redirects and the callback are
        // not part of the documented JSON API surface)
        //  GET /auth/github/login
        .route("/auth/github/login", get(auth::github_login))
        //  GET /auth/github/callback
        .route("/auth/github/callback", get(auth::github_callback))
        //  GET /auth/whoami
        .route("/auth/whoami", get(auth::whoami))
        // job management group
        //  POST /jobs/new
        // .api_route("/jobs/new", post_with(jobs::submit, |o| o))
        //  GET /jobs (+ <FILTERS>)
        // .api_route("/jobs", get_with(jobs::list, |o| o))
        //  GET /jobs/{id}/events
        .api_route("/jobs/:id/events", get_with(jobs::list_events, |o| o))
        //  GET /jobs/{id}/status
        // .api_route("/jobs/{id}/status", get_with(jobs::status, |o| o))
        //  GET /jobs/{id}/info
        //  DELETE /jobs/{id}
        // .api_route("/jobs/{id}", delete_with(jobs::stop, |o| o))
        // supervisor management group
        //  GET /supervisors (+ <FILTERS>)
        // .api_route("/supervisors", get_with(supervisors::list, |o| o))
        //  GET /supervisors/{id}/status
        // .api_route(
        //     "/supervisors/{id}/status",
        //     get_with(supervisors::status, |o| o),
        // )
        //  GET /supervisors/{id}/current-job
        //  DELETE /supervisors/{id}/current-job
        //  POST /supervisors/new
        //  DELETE /supervisors/{id}
        //  GET /hosts/{id}/events
        .api_route("/hosts/:id/events", get_with(hosts::list_events, |o| o))
        //  GET /supervisors/{id}/connect
        // Note that the HTTP verb 'GET' here is not necessarily conformant with REST principles,
        // but is required by RFC6455 §4.1: "The method of the request MUST be GET" (regarding
        // WebSocket HTTP handshakes).
        .api_route("/hosts/:id/connect", get_with(hosts::connect, |o| o))
    // user management group
    //  GET /users/{id}/events
    .api_route("/users/:id/events", get_with(users::list_events, |o| o))
    //  GET /users/{id}/jobs (+ <FILTERS>)
    //  GET /users/{id}/tokens (+ <FILTERS>)
    // token management group
    //  POST /tokens/new
    //  DELETE /tokens/{id}
}

async fn not_found() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "no such route")
}

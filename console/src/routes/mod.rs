//! HTTP routes for the console.
//!
//! The surface is deliberately tiny: an entry redirect, the login-flow
//! endpoints (`/login` → switchboard, `/auth/complete` and `/auth/landing` ←
//! switchboard, `/logout`), the per-resource pages, and the embedded
//! stylesheet.

mod auth;
mod jobs;
mod me;
mod statics;

use axum::Router;
use axum::routing::{get, post};
use tower_http::trace::TraceLayer;

use crate::serve::AppState;

/// Path of the login-landing route, mounted below. Public so the switchboard
/// can derive the embedded console's implicitly-allowed `return_to` URL from
/// the same definition the console routes by.
pub const LANDING_PATH: &str = "/auth/landing";

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(me::index))
        // login flow
        .route("/login", get(auth::login))
        .route("/auth/complete", get(auth::complete))
        .route(LANDING_PATH, get(auth::landing))
        .route("/logout", get(auth::logout))
        // resource pages
        .route("/me", get(me::me))
        .route("/users/{id}", get(me::user))
        // job pages
        .route("/jobs", get(jobs::list))
        .route("/jobs/new", get(jobs::new_form).post(jobs::dispatch))
        .route("/jobs/{id}", get(jobs::show))
        .route("/jobs/{id}/terminate", post(jobs::terminate))
        // assets
        .route("/static/style.css", get(statics::style))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

//! HTTP routes for the console.
//!
//! The surface is deliberately tiny: an entry redirect, the three login-flow
//! endpoints (`/login` → switchboard, `/auth/landing` ← switchboard, `/logout`),
//! the per-resource pages, and the embedded stylesheet.

mod auth;
mod jobs;
mod me;
mod statics;

use axum::Router;
use axum::routing::{get, post};
use tower_http::trace::TraceLayer;

use crate::serve::AppState;

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(me::index))
        // login flow
        .route("/login", get(auth::login))
        .route("/auth/landing", get(auth::landing))
        .route("/logout", get(auth::logout))
        // resource pages
        .route("/me", get(me::me))
        .route("/users/{id}", get(me::user))
        // job pages
        .route("/jobs", get(jobs::list))
        .route("/jobs/{id}", get(jobs::show))
        .route("/jobs/{id}/terminate", post(jobs::terminate))
        // assets
        .route("/static/style.css", get(statics::style))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

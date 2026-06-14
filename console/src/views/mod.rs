//! Maud view helpers: the page chrome shared by every route, plus small
//! formatting utilities and the error response type.
//!
//! Reusable per-resource components (e.g. the audit feed) live in submodules so
//! every resource page can drop them in identically.

pub mod audit;
pub mod jobs;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, Utc};
use maud::{DOCTYPE, Markup, html};
use treadmill_rs::api::switchboard::client::ClientError;

/// Wrap page content in the shared HTML shell: doctype, stylesheet, top nav,
/// and footer. `current` is the logged-in user's handle, shown in the nav when
/// present.
pub fn layout(title: &str, current: Option<&str>, content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · treadmill" }
                link rel="stylesheet" href="/static/style.css";
            }
            body {
                header.site {
                    nav {
                        a.brand href="/" { "treadmill console" }
                        span.spacer {}
                        @if let Some(user) = current {
                            a href="/jobs" { "jobs" }
                            a href="/me" { (user) }
                            a href="/logout" { "log out" }
                        } @else {
                            a href="/login" { "log in" }
                        }
                    }
                }
                main { (content) }
                footer.site { "treadmill — distributed hardware testbed" }
            }
        }
    }
}

/// Render an absolute UTC timestamp as a `<time>` element.
pub fn timestamp(ts: DateTime<Utc>) -> Markup {
    let machine = ts.to_rfc3339();
    let human = ts.format("%Y-%m-%d %H:%M UTC").to_string();
    html! { time datetime=(machine) { (human) } }
}

/// The error type page handlers return. A switchboard `Unauthorized` becomes a
/// redirect to `/login` (the token is gone or expired); anything else renders a
/// minimal error page with the appropriate status.
pub enum PageError {
    Unauthorized,
    Internal(String),
    Status(StatusCode, String),
}

impl From<ClientError> for PageError {
    fn from(err: ClientError) -> Self {
        match err {
            ClientError::Unauthorized => PageError::Unauthorized,
            ClientError::Status { status, body } => PageError::Status(
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                body,
            ),
            other => PageError::Internal(other.to_string()),
        }
    }
}

impl IntoResponse for PageError {
    fn into_response(self) -> Response {
        match self {
            PageError::Unauthorized => Redirect::to("/login").into_response(),
            PageError::Internal(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_page("Something went wrong", &msg),
            )
                .into_response(),
            PageError::Status(status, body) => {
                (status, error_page("Request failed", &body)).into_response()
            }
        }
    }
}

fn error_page(heading: &str, detail: &str) -> Markup {
    layout(
        heading,
        None,
        html! {
            h1 { (heading) }
            section.card {
                p.muted { "The switchboard could not satisfy this request." }
                @if !detail.is_empty() {
                    pre { code { (detail) } }
                }
            }
        },
    )
}

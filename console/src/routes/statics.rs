//! Embedded static assets.

use axum::http::header;
use axum::response::IntoResponse;

use crate::assets::STYLE_CSS;

/// `GET /static/style.css` — the console stylesheet.
pub async fn style() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        STYLE_CSS,
    )
}

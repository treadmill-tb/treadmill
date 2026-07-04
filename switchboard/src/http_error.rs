//! Shared helpers for turning unexpected errors into HTTP `500` responses.
//!
//! Route handlers carry their errors as a bare [`StatusCode`]. An *unexpected*
//! failure — almost always a database error — should be logged once, server
//! side, and surfaced to the client as an opaque `500` that leaks nothing.
//! Centralizing that here keeps the handlers free of the repeated
//! `map_err(|e| { tracing::error!(...); INTERNAL_SERVER_ERROR })` boilerplate
//! and gives every internal failure a single, consistent log shape.

use std::fmt::Display;

use http::StatusCode;

/// Log `e` and map it to a `500 Internal Server Error`.
///
/// Use as a `map_err` argument for an unexpected error whose own `Display`
/// already says enough (`foo().await.map_err(internal)?`), or to mint a `500`
/// from a violated invariant (`row.ok_or_else(|| internal("user vanished"))?`).
/// When the failing *operation* needs naming for the log, prefer
/// [`OrInternal::or_internal`] instead.
pub(crate) fn internal(e: impl Display) -> StatusCode {
    tracing::error!("internal error: {e}");
    StatusCode::INTERNAL_SERVER_ERROR
}

/// Collapse the "log the error, return a `500`" pattern on a `Result` into one
/// call that also records *what* was being attempted.
pub(crate) trait OrInternal<T> {
    /// Map any error to a logged `500`, prefixing the log line with `context` —
    /// a short description of the operation that failed (e.g.
    /// `"inserting enqueued job"`).
    fn or_internal(self, context: &str) -> Result<T, StatusCode>;
}

impl<T, E: Display> OrInternal<T> for Result<T, E> {
    fn or_internal(self, context: &str) -> Result<T, StatusCode> {
        self.map_err(|e| {
            tracing::error!("{context}: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })
    }
}

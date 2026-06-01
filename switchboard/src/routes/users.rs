use axum::extract::{Path, State};
use axum::Json;
use http::StatusCode;
use uuid::Uuid;

use crate::audit::feed::{AuditFeedResponse, fetch_events_for_entity};
use crate::serve::AppState;

/// Axum handler for the `/users/{id}/events` path.
pub async fn list_events(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(user_id): Path<Uuid>,
) -> Result<Json<AuditFeedResponse>, StatusCode> {
    fetch_events_for_entity(&state, &subject, "subject", user_id)
        .await
        .map(Json)
}

use crate::server::AppState;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use uuid::Uuid;

pub async fn list(State(app_state): State<AppState>) -> Response {
    let supervisor_ids: Vec<Uuid> = app_state
        .scheduler()
        .herd()
        .lock_supervisors()
        .await
        .keys()
        .copied()
        .collect();
    Json(supervisor_ids).into_response()
}

pub async fn status(
    State(app_state): State<AppState>,
    Path(supervisor_id): Path<Uuid>,
) -> Response {
    app_state
        .scheduler()
        .herd()
        .supervisor_status(supervisor_id)
        .await
        .map_err(|e| format!("{e}"))
        .map(Json)
        .into_response()
}

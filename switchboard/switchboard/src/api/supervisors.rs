use crate::server::AppState;
use axum::extract::State;
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
        .collect();
    Json(supervisor_ids).into_response()
}

pub async fn status() -> Response {
    todo!()
}

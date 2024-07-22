use super::{BifurcateProxy, IntoProxiedResponse, ResponseProxy};
use crate::perms::jobs::{enqueue_ci_job, EnqueueCIJobAction, EnqueueJobError};
use crate::server::auth::{AuthSource, AuthorizationError, AuthorizationSource, DbPermSource};
use crate::server::AppState;
use crate::supervisor::HerdError;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use http::StatusCode;
use treadmill_rs::api::switchboard::{EnqueueJobRequest, EnqueueJobResponse};
// POST /job/queue

impl IntoProxiedResponse for EnqueueJobResponse {
    fn into_proxied_response(self) -> Response {
        match self {
            EnqueueJobResponse::Ok => StatusCode::OK.into_response(),
            EnqueueJobResponse::SupervisorNotFound => StatusCode::NOT_FOUND.into_response(),
            EnqueueJobResponse::Unauthorized => StatusCode::UNAUTHORIZED.into_response(),
            this @ EnqueueJobResponse::Invalid { .. } => {
                (StatusCode::BAD_REQUEST, Json(this)).into_response()
            }
            EnqueueJobResponse::Internal => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            EnqueueJobResponse::Conflict => StatusCode::CONFLICT.into_response(),
        }
    }
}
impl From<EnqueueJobError> for EnqueueJobResponse {
    fn from(value: EnqueueJobError) -> Self {
        match value {
            EnqueueJobError::Database => EnqueueJobResponse::Internal,
            EnqueueJobError::SupervisorNotFound => EnqueueJobResponse::SupervisorNotFound,
            EnqueueJobError::Herd(e) => match e {
                HerdError::InvalidSupervisor => EnqueueJobResponse::SupervisorNotFound,
                HerdError::BusySupervisor => EnqueueJobResponse::Conflict,
            },
        }
    }
}
pub async fn enqueue(
    AuthSource(auth_source): AuthSource<DbPermSource>,
    State(state): State<AppState>,
    Json(request): Json<EnqueueJobRequest>,
) -> BifurcateProxy<EnqueueJobResponse> {
    // check that the supervisor exists (we don't check if it's online)

    let privilege = auth_source
        .authorize(EnqueueCIJobAction {
            supervisor_id: request.supervisor_id,
        })
        .await
        .map_err(|e| match e {
            AuthorizationError::Database(e) => {
                tracing::error!("failed to check privilege: {e}");
                EnqueueJobResponse::Internal
            }
            AuthorizationError::Unauthorized(p) => {
                tracing::warn!("{auth_source:?} lacks permission {p}");
                EnqueueJobResponse::Unauthorized
            }
        })
        .map_err(ResponseProxy)?;

    enqueue_ci_job(
        &state,
        privilege,
        request.start_job_request,
        request.supervisor_id,
    )
    .await
    .map_err(|e| ResponseProxy(EnqueueJobResponse::from(e)))?;

    super::bifurcated_ok!(EnqueueJobResponse::Ok)
}

// GET /job/queue

pub async fn get_queue() {
    unimplemented!()
}

// GET /job/:id/info

pub async fn info() {
    unimplemented!()
}

// GET /job/:id/status

pub async fn status() {
    unimplemented!()
}

// DELETE /job/:id

pub async fn cancel() {
    unimplemented!()
}

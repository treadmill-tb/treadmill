use super::{BifurcateProxy, IntoProxiedResponse, ResponseProxy};
use crate::perms::jobs::{
    enqueue_ci_job, read_job_status, EnqueueCIJobAction, EnqueueJobError, JobStatusAction,
    JobStatusError,
};
use crate::server::auth::{AuthSource, AuthorizationError, AuthorizationSource, DbPermSource};
use crate::server::AppState;
use crate::supervisor::{HerdError, JobMarketError};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::{extract, Json};
use http::StatusCode;
use treadmill_rs::api::switchboard::{EnqueueJobRequest, EnqueueJobResponse, JobStatusResponse};
use uuid::Uuid;

impl IntoProxiedResponse for EnqueueJobResponse {
    fn into_proxied_response(self) -> Response {
        let status_code = match &self {
            EnqueueJobResponse::Ok => StatusCode::OK,
            EnqueueJobResponse::SupervisorNotFound => StatusCode::NOT_FOUND,
            EnqueueJobResponse::Unauthorized => StatusCode::UNAUTHORIZED,
            EnqueueJobResponse::Invalid { .. } => StatusCode::BAD_REQUEST,
            EnqueueJobResponse::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            EnqueueJobResponse::Conflict => StatusCode::CONFLICT,
        };
        (status_code, Json(self)).into_response()
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

    enqueue_ci_job(&state, privilege, request.start_job_request)
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
impl IntoProxiedResponse for JobStatusResponse {
    fn into_proxied_response(self) -> Response {
        let status_code = match &self {
            JobStatusResponse::Ok { .. } => StatusCode::OK,
            JobStatusResponse::JobNotFound => StatusCode::NOT_FOUND,
            JobStatusResponse::Unauthorized => StatusCode::UNAUTHORIZED,
            JobStatusResponse::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status_code, Json(self)).into_response()
    }
}
impl From<JobStatusError> for JobStatusResponse {
    fn from(value: JobStatusError) -> Self {
        match value {
            JobStatusError::Database => JobStatusResponse::Internal,
            JobStatusError::JobNotFound => JobStatusResponse::JobNotFound,
            JobStatusError::Herd(e) => match e {
                // TODO: better error for this case
                HerdError::InvalidSupervisor => JobStatusResponse::Internal,
                // shouldn't happen
                HerdError::BusySupervisor => JobStatusResponse::Internal,
            },
            JobStatusError::JobMarket(e) => match e {
                JobMarketError::InvalidJob => JobStatusResponse::JobNotFound,
            },
        }
    }
}
pub async fn status(
    AuthSource(auth_source): AuthSource<DbPermSource>,
    State(state): State<AppState>,
    extract::Path(job_id): extract::Path<Uuid>,
) -> BifurcateProxy<JobStatusResponse> {
    let privilege = auth_source
        .authorize(JobStatusAction { job_id })
        .await
        .map_err(|e| match e {
            AuthorizationError::Database(e) => {
                tracing::error!("failed to check privilege: {e}");
                JobStatusResponse::Internal
            }
            AuthorizationError::Unauthorized(p) => {
                tracing::warn!("{auth_source:?} lacks permission {p}");
                JobStatusResponse::Unauthorized
            }
        })
        .map_err(ResponseProxy)?;

    read_job_status(&state, privilege)
        .await
        .map(|job_status| JobStatusResponse::Ok { job_status })
        .map(ResponseProxy)
        .map_err(JobStatusResponse::from)
        .map_err(ResponseProxy)
}

// DELETE /job/:id
pub async fn cancel() {
    unimplemented!()
}

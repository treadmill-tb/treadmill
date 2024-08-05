use super::{BifurcateProxy, JsonProxiedResponse, ResponseProxy};
use crate::herd::HerdError;
use crate::jobs::{JobError, KanbanError};
use crate::perms::jobs::{enqueue_ci_job, EnqueueCIJobAction, EnqueueJobError};
use crate::sched::SchedError;
use crate::server::auth::{AuthSource, AuthorizationError, AuthorizationSource, DbPermSource};
use crate::server::AppState;
use axum::extract::State;
use axum::Json;
use http::StatusCode;
use treadmill_rs::api::switchboard::{EnqueueJobRequest, EnqueueJobResponse};

impl JsonProxiedResponse for EnqueueJobResponse {
    fn status_code(&self) -> StatusCode {
        match &self {
            EnqueueJobResponse::Ok { .. } => StatusCode::OK,
            EnqueueJobResponse::SupervisorNotFound => StatusCode::NOT_FOUND,
            EnqueueJobResponse::Unauthorized => StatusCode::UNAUTHORIZED,
            EnqueueJobResponse::Invalid { .. } => StatusCode::BAD_REQUEST,
            EnqueueJobResponse::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            EnqueueJobResponse::Conflict => StatusCode::CONFLICT,
        }
    }
}
impl From<EnqueueJobError> for EnqueueJobResponse {
    fn from(value: EnqueueJobError) -> Self {
        match value {
            EnqueueJobError::Database => EnqueueJobResponse::Internal,
            EnqueueJobError::Scheduler(s) => match s {
                SchedError::Kanban(kanban_error) => match kanban_error {
                    KanbanError::JobAlreadyExists => EnqueueJobResponse::Conflict,
                    KanbanError::NoSuchJob => unreachable!(),
                },
                SchedError::Herd(herd_error) => match herd_error {
                    HerdError::SupervisorAlreadyConnected => unreachable!(),
                    HerdError::SupervisorNotConnected => EnqueueJobResponse::SupervisorNotFound,
                    HerdError::NoSuchSupervisor => EnqueueJobResponse::SupervisorNotFound,
                    HerdError::SupervisorAlreadyReserved => EnqueueJobResponse::Conflict,
                },
                SchedError::Job(job_error) => match job_error {
                    JobError::JobAlreadyActive => EnqueueJobResponse::Conflict,
                },
                SchedError::FailedToSend => EnqueueJobResponse::SupervisorNotFound,
                SchedError::InvalidTimeout => EnqueueJobResponse::Invalid {
                    reason: "invalid timeout".to_owned(),
                },
                SchedError::Database(_) => EnqueueJobResponse::Internal,
            },
        }
    }
}
pub async fn enqueue(
    AuthSource(auth_source): AuthSource<DbPermSource>,
    State(state): State<AppState>,
    Json(request): Json<EnqueueJobRequest>,
) -> BifurcateProxy<EnqueueJobResponse> {
    let privilege = auth_source
        .authorize(EnqueueCIJobAction {
            // supervisor_id: request.supervisor_id,
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

    let job_id = enqueue_ci_job(&state, privilege, request.job_request, auth_source.clone())
        .await
        .map_err(|e| ResponseProxy(EnqueueJobResponse::from(e)))?;

    super::bifurcated_ok!(EnqueueJobResponse::Ok { job_id })
}

// GET /job/queue
pub async fn get_queue() {
    unimplemented!()
}

// GET /job/:id/info
pub async fn info() {
    unimplemented!()
}

// // GET /job/:id/status
// impl IntoProxiedResponse for JobStatusResponse {
//     fn into_proxied_response(self) -> Response {
//         let status_code = match &self {
//             JobStatusResponse::Ok { .. } => StatusCode::OK,
//             JobStatusResponse::JobNotFound => StatusCode::NOT_FOUND,
//             JobStatusResponse::Unauthorized => StatusCode::UNAUTHORIZED,
//             JobStatusResponse::Internal => StatusCode::INTERNAL_SERVER_ERROR,
//             JobStatusResponse::SupervisorNotFound => StatusCode::NOT_FOUND,
//         };
//         (status_code, Json(self)).into_response()
//     }
// }
// impl From<JobStatusError> for JobStatusResponse {
//     fn from(value: JobStatusError) -> Self {
//         match value {
//             JobStatusError::JobNotFound => JobStatusResponse::JobNotFound,
//             JobStatusError::Herd(e) => match e {
//                 // TODO: better error for this case
//                 HerdError::InvalidSupervisor => JobStatusResponse::SupervisorNotFound,
//                 // shouldn't happen
//                 HerdError::BusySupervisor => JobStatusResponse::Internal,
//             },
//             JobStatusError::JobMarket(e) => match e {
//                 JobMarketError::InvalidJob => JobStatusResponse::JobNotFound,
//                 JobMarketError::Disconnect => JobStatusResponse::SupervisorNotFound,
//             },
//         }
//     }
// }
pub async fn status() {}
//     AuthSource(auth_source): AuthSource<DbPermSource>,
//     State(state): State<AppState>,
//     extract::Path(job_id): extract::Path<Uuid>,
// ) -> BifurcateProxy<JobStatusResponse> {
//     let privilege = auth_source
//         .authorize(JobStatusAction { job_id })
//         .await
//         .map_err(|e| match e {
//             AuthorizationError::Database(e) => {
//                 tracing::error!("failed to check privilege: {e}");
//                 JobStatusResponse::Internal
//             }
//             AuthorizationError::Unauthorized(p) => {
//                 tracing::warn!("{auth_source:?} lacks permission {p}");
//                 JobStatusResponse::Unauthorized
//             }
//         })
//         .map_err(ResponseProxy)?;
//
//     read_job_status(&state, privilege)
//         .await
//         .map(|job_status| JobStatusResponse::Ok { job_status })
//         .map(ResponseProxy)
//         .map_err(JobStatusResponse::from)
//         .map_err(ResponseProxy)
// }
//
// // DELETE /job/:id
// impl JsonProxiedResponse for JobCancelResponse {
//     fn status_code(&self) -> StatusCode {
//         match self {
//             JobCancelResponse::Ok => StatusCode::OK,
//             JobCancelResponse::JobNotFound => StatusCode::NOT_FOUND,
//             JobCancelResponse::Unauthorized => StatusCode::UNAUTHORIZED,
//             JobCancelResponse::Internal => StatusCode::INTERNAL_SERVER_ERROR,
//             JobCancelResponse::SupervisorNotFound => StatusCode::NOT_FOUND,
//         }
//     }
// }
// impl From<StopJobError> for JobCancelResponse {
//     fn from(value: StopJobError) -> Self {
//         // TODO: redo this
//         match value {
//             StopJobError::JobNotFound => JobCancelResponse::JobNotFound,
//             StopJobError::Herd(e) => match e {
//                 HerdError::InvalidSupervisor => {
//                     // shouldn't happen?
//                     panic!()
//                 }
//                 HerdError::BusySupervisor => {
//                     // shouldn't happen?
//                     panic!()
//                 }
//             },
//             StopJobError::JobMarket(e) => match e {
//                 JobMarketError::InvalidJob => JobCancelResponse::JobNotFound,
//                 JobMarketError::Disconnect => JobCancelResponse::JobNotFound,
//             },
//         }
//     }
// }
pub async fn cancel() {}
//     AuthSource(auth_source): AuthSource<DbPermSource>,
//     State(state): State<AppState>,
//     extract::Path(job_id): extract::Path<Uuid>,
// ) -> BifurcateProxy<JobCancelResponse> {
//     let privilege = auth_source
//         .authorize(StopJobAction { job_id })
//         .await
//         .map_err(|e| match e {
//             AuthorizationError::Database(e) => {
//                 tracing::error!("failed to check privilege: {e}");
//                 JobCancelResponse::Internal
//             }
//             AuthorizationError::Unauthorized(p) => {
//                 tracing::warn!("{auth_source:?} lacks permission {p}");
//                 JobCancelResponse::Unauthorized
//             }
//         })
//         .map_err(ResponseProxy)?;
//
//     stop_job(&state, privilege)
//         .await
//         .map(|()| JobCancelResponse::Ok)
//         .map(ResponseProxy)
//         .map_err(JobCancelResponse::from)
//         .map_err(ResponseProxy)
// }

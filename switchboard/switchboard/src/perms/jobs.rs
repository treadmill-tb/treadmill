//! Privileged actions acting on jobs and the job queue.

use crate::model::{Job, JobParameter};
use crate::perms::{BifurcateProxy, IntoProxiedResponse, ResponseProxy};
use crate::server::auth::{
    AuthSource, AuthorizationError, AuthorizationSource, DbPermSource, PermissionQueryExecutor,
    Privilege, PrivilegedAction,
};
use crate::server::AppState;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::{async_trait, Json};
use http::StatusCode;
use std::fmt::Debug;
use treadmill_rs::api::switchboard::{EnqueueJobRequest, EnqueueJobResponse};
use treadmill_rs::connector::StartJobRequest;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct EnqueueCIJobAction {
    pub supervisor_id: Uuid,
}
#[async_trait]
impl PrivilegedAction for EnqueueCIJobAction {
    async fn authorize<'s, PQE: PermissionQueryExecutor + Send>(
        self,
        perm_query_exec: PQE,
    ) -> Result<Privilege<'s, Self>, AuthorizationError> {
        perm_query_exec
            .query(format!("enqueue_ci_job:{}", &self.supervisor_id))
            .await
            .try_into_privilege(self)
    }
}

#[derive(Debug)]
pub enum EnqueueJobError {
    Database,
    SupervisorNotFound,
}
pub async fn enqueue_ci_job(
    state: &AppState,
    p: Privilege<'_, EnqueueCIJobAction>,
    job: StartJobRequest,
) -> Result<(), EnqueueJobError> {
    let mut transaction = state.pool().begin().await.map_err(|e| {
        tracing::error!("Failed to create transaction: {e}");
        EnqueueJobError::Database
    })?;
    Job::insert(&job, p.subject(), transaction.as_mut())
        .await
        .map_err(|e| {
            tracing::error!("failed to add job ({}) to transaction: {e}", job.job_id);
            EnqueueJobError::Database
        })?;
    let job_id = job.job_id;
    JobParameter::insert(job_id, job.parameters, transaction.as_mut())
        .await
        .map_err(|e| {
            tracing::error!(
                "failed to add job ({}) parameters to transaction: {e}",
                job_id
            );
            EnqueueJobError::Database
        })?;
    transaction.commit().await.map_err(|e| {
        tracing::error!("failed to commit transaction: {e}");
        EnqueueJobError::Database
    })?;

    let subject = p.subject();
    let object = &p.action().supervisor_id;
    println!("[subject:{subject:?}] enqueuing a job on [object:{object:?}]");

    Ok(())
}

// ENDPOINTS

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
        }
    }
}
impl From<EnqueueJobError> for EnqueueJobResponse {
    fn from(value: EnqueueJobError) -> Self {
        match value {
            EnqueueJobError::Database => EnqueueJobResponse::Internal,
            EnqueueJobError::SupervisorNotFound => EnqueueJobResponse::SupervisorNotFound,
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

pub async fn get_queue() {
    unimplemented!()
}
pub async fn info() {
    unimplemented!()
}
pub async fn cancel() {
    unimplemented!()
}
pub async fn status() {
    unimplemented!()
}

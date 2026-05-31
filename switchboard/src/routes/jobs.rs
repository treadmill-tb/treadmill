use crate::auth::AuthorizationSource;
use crate::auth::db::DbAuth;
use crate::auth::extract::AuthSource;
use crate::perms::{ReadJobStatusError, StopJobError, SubmitJobError};
use crate::routes::proxy::{Proxied, proxy_err, proxy_val};
use crate::serve::AppState;
use crate::{impl_from_auth_err, perms};
use aide::{OperationInput, OperationOutput};
use axum::Json;
use axum::extract::{Path, State};
use futures_util::StreamExt;
use futures_util::stream::FuturesOrdered;
use treadmill_rs::api::switchboard::jobs::{
    list::Response as LJResponse,
    status::Response as JSResponse,
    stop::Response as SJResponse,
    submit::{Request as EJRequest, Response as EJResponse},
};
use uuid::Uuid;

// -- submit

impl_from_auth_err!(EJResponse, Database => Internal, Unauthorized => Unauthorized);
#[tracing::instrument(skip(state, auth))]
pub async fn submit(
    State(state): State<AppState>,
    AuthSource(auth): AuthSource<DbAuth>,
    Json(request): Json<EJRequest>,
) -> Proxied<EJResponse> {
    let submit_job_priv = auth.authorize(perms::SubmitJob).await.map_err(proxy_err)?;
    let job_id = perms::submit_job(&state, submit_job_priv, request.job_request)
        .await
        .map_err(|e| {
            proxy_err(match e {
                SubmitJobError::Internal => EJResponse::Internal,
                SubmitJobError::SupervisorMatchError => EJResponse::SupervisorMatchError,
            })
        })?;

    proxy_val(EJResponse::Ok { job_id })
}

// -- status

impl_from_auth_err!(JSResponse, Database => Internal, Unauthorized => Invalid);
#[tracing::instrument(skip(state, auth))]
pub async fn status(
    State(state): State<AppState>,
    AuthSource(auth): AuthSource<DbAuth>,
    Path(job_id): Path<Uuid>,
) -> Proxied<JSResponse> {
    let read = auth
        .authorize(perms::ReadJobStatus { job_id })
        .await
        .map_err(proxy_err)?;
    let job_status = perms::read_job_status(&state, read)
        .await
        .map_err(|e| match e {
            ReadJobStatusError::Internal => JSResponse::Internal,
            ReadJobStatusError::NoSuchJob => JSResponse::Invalid,
        })
        .map_err(proxy_err)?;

    proxy_val(JSResponse::Ok { job_status })
}

// -- stop

impl_from_auth_err!(SJResponse, Database => Internal, Unauthorized => Invalid);
#[tracing::instrument(skip(state, auth))]
pub async fn stop(
    State(state): State<AppState>,
    AuthSource(auth): AuthSource<DbAuth>,
    Path(job_id): Path<Uuid>,
) -> Proxied<SJResponse> {
    let stop = auth
        .authorize(perms::StopJob { job_id })
        .await
        .map_err(proxy_err)?;
    perms::stop_job(&state, stop)
        .await
        .map_err(|e| match e {
            StopJobError::Internal => SJResponse::Internal,
            StopJobError::NoSuchJob => SJResponse::Invalid,
        })
        .map_err(proxy_err)?;

    proxy_val(SJResponse::Ok)
}

// -- list
impl_from_auth_err!(LJResponse, Database => Internal, Unauthorized => Unauthorized);
#[tracing::instrument(skip(state, auth))]
pub async fn list(
    State(state): State<AppState>,
    AuthSource(auth): AuthSource<DbAuth>,
) -> Proxied<LJResponse> {
    let list_jobs = auth.authorize(perms::ListJobs).await.map_err(proxy_err)?;
    // Placeholder listing under the permissive authorization shim: a subject sees
    // the jobs it owns. The grant-aware version (jobs readable via group
    // membership or explicit grants) lands with the full authorization rewrite.
    let job_ids: Vec<Uuid> = sqlx::query_scalar!(
        r#"select job_id from tml_switchboard.jobs where owner_id = $1"#,
        list_jobs.subject().user_id()
    )
    .fetch_all(state.pool())
    .await
    .map_err(|_| LJResponse::Internal)
    .map_err(proxy_err)?;

    let perm_queries: FuturesOrdered<_> = job_ids
        .into_iter()
        .map(|job_id| auth.authorize(perms::ReadJobStatus { job_id }))
        .collect();
    let job_perms: Vec<_> = perm_queries
        .filter_map(|x| async move { x.ok() })
        .collect()
        .await;
    let jobs = perms::list_jobs(&state, list_jobs, job_perms).await;
    proxy_val(LJResponse::Ok { jobs })
}

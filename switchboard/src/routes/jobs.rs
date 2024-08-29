use crate::auth::db::DbAuth;
use crate::auth::extract::AuthSource;
use crate::auth::AuthorizationSource;
use crate::perms::{read_supervisor_status, ReadJobStatusError, StopJobError, SubmitJobError};
use crate::routes::proxy::{proxy_err, proxy_val, Proxied};
use crate::serve::AppState;
use crate::{impl_from_auth_err, perms, sql};
use axum::extract::{Path, State};
use axum::Json;
use treadmill_rs::api::switchboard::jobs::{
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
    let user_id = submit_job_priv.subject().user_id();
    let job_id = perms::submit_job(&state, submit_job_priv, request.job_request)
        .await
        .map_err(|e| {
            proxy_err(match e {
                SubmitJobError::Internal => EJResponse::Internal,
                SubmitJobError::FailedToMatch => EJResponse::FailedToMatch,
            })
        })?;
    sql::perm::sql_add_user_privileges(
        user_id,
        vec![
            format!("read_job_status:{job_id}"),
            format!("stop_job:{job_id}"),
        ],
        state.pool(),
    )
    .await
    .map_err(|e| EJResponse::Internal)
    .map_err(proxy_err)?;

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

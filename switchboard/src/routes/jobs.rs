use crate::auth::AuthorizationSource;
use crate::auth::db::DbAuth;
use crate::auth::extract::AuthSource;
use crate::perms::{ReadJobStatusError, StopJobError, SubmitJobError};
use crate::routes::proxy::{Proxied, proxy_err, proxy_val};
use crate::serve::AppState;
use crate::{impl_from_auth_err, perms, sql};
use axum::Json;
use axum::extract::{Path, State};
use futures_util::StreamExt;
use futures_util::stream::FuturesOrdered;
use std::str::FromStr;
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
    let user_id = submit_job_priv.subject().user_id();
    let job_id = perms::submit_job(&state, submit_job_priv, request.job_request)
        .await
        .map_err(|e| {
            proxy_err(match e {
                SubmitJobError::Internal => EJResponse::Internal,
                SubmitJobError::SupervisorMatchError => EJResponse::SupervisorMatchError,
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
    .map_err(|_| EJResponse::Internal)
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

// -- list
impl_from_auth_err!(LJResponse, Database => Internal, Unauthorized => Unauthorized);
#[tracing::instrument(skip(state, auth))]
pub async fn list(
    State(state): State<AppState>,
    AuthSource(auth): AuthSource<DbAuth>,
) -> Proxied<LJResponse> {
    let list_jobs = auth.authorize(perms::ListJobs).await.map_err(proxy_err)?;
    // TODO: would be better to make this a method on DbAuth
    // TODO: respect token privileges
    let perms = sqlx::query!(
        r#"
        select permission from tml_switchboard.user_privileges
        where user_id = $1 and permission like 'read_job_status:%';
        "#,
        list_jobs.subject().user_id()
    )
    .fetch_all(state.pool())
    .await
    .map_err(|_| LJResponse::Internal)
    .map_err(proxy_err)?;
    let job_ids: Vec<Uuid> = perms
        .into_iter()
        .map(|perm| Uuid::from_str(&perm.permission.split(':').skip(1).next().unwrap()).unwrap())
        .collect();

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

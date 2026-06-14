use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use http::StatusCode;
use uuid::Uuid;

use treadmill_rs::api::switchboard::jobs::{JobInfo, LogStreamCredentials};

use crate::audit::feed::{AuditFeedResponse, fetch_events_for_entity};
use crate::auth::engine::{self, JobPermission};
use crate::log_streaming::{self, TokenScope};
use crate::serve::AppState;
use crate::sql::job;

/// Read tokens are deliberately short-lived. A NATS bearer JWT is only checked
/// at connect time — an already-established connection is not dropped when the
/// token expires — so a tight TTL bounds a leaked token's exposure without
/// disrupting an in-progress live tail. Clients re-request on reconnect.
const READ_TOKEN_TTL: Duration = Duration::from_secs(5 * 60);

/// Axum handler for the `/jobs/{id}/events` path.
pub async fn list_events(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(job_id): Path<Uuid>,
) -> Result<Json<AuditFeedResponse>, StatusCode> {
    fetch_events_for_entity(&state, &subject, "job", job_id)
        .await
        .map(Json)
}

/// Axum handler for `GET /jobs/{id}`.
///
/// Returns the full [`JobInfo`] view of a job, gated on the caller's `read`
/// permission. A caller who cannot read the job — including the case where the
/// job does not exist — gets `403` rather than a signal of the job's
/// (non-)existence, matching the log-token route.
pub async fn get_job(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(job_id): Path<Uuid>,
) -> Result<Json<JobInfo>, StatusCode> {
    let authorized =
        engine::can_access_job(state.pool(), subject.user_id(), job_id, JobPermission::Read)
            .await
            .map_err(|e| {
                tracing::error!("checking job read access for get_job: {e}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
    if !authorized {
        return Err(StatusCode::FORBIDDEN);
    }

    let mut conn = state.pool().acquire().await.map_err(|e| {
        tracing::error!("acquiring a connection for get_job: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let sql_job = job::fetch_by_job_id(job_id, &mut *conn)
        .await
        .map_err(|e| {
            // The `read` check above already passed, so the row exists; a missing row
            // here is a genuine internal inconsistency, not a 404.
            tracing::error!("fetching job {job_id} for get_job: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let info = sql_job.into_info(&mut conn).await.map_err(|e| {
        tracing::error!("rendering job {job_id} into JobInfo: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(info))
}

/// Axum handler for `POST /jobs/{id}/log-token`.
///
/// Mints a short-lived, subscribe-scoped NATS **bearer** token for tailing or
/// replaying a job's console logs, gated on the caller's `read` permission for
/// the job. Returns the NATS URL, the subject to subscribe to, the token, and
/// its lifetime.
pub async fn log_token(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(job_id): Path<Uuid>,
) -> Result<Json<LogStreamCredentials>, StatusCode> {
    // Gate on the job's `read` permission (owner, an explicit read grant, or a
    // global admin). A job that does not exist yields `false` here, so the
    // caller gets 403 rather than a signal of the job's (non-)existence.
    let authorized =
        engine::can_access_job(state.pool(), subject.user_id(), job_id, JobPermission::Read)
            .await
            .map_err(|e| {
                tracing::error!("checking job read access for a log token: {e}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
    if !authorized {
        return Err(StatusCode::FORBIDDEN);
    }

    // Log streaming may be turned off in this deployment; the feature exists but
    // is unavailable here.
    let log_streaming = state
        .log_streaming()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let token = log_streaming::mint_token(
        &log_streaming.config,
        job_id,
        TokenScope::Subscribe,
        Some(READ_TOKEN_TTL),
    )
    .map_err(|e| {
        tracing::error!("minting a log read token: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(LogStreamCredentials {
        nats_url: log_streaming.config.nats_url.clone(),
        subject: log_streaming::subject_scope(job_id),
        token,
        expires_in_secs: READ_TOKEN_TTL.as_secs(),
    }))
}

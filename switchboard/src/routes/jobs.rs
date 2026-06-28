use std::time::Duration;

use axum::Json;
use axum::extract::Path;
use axum::extract::{Query, State};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::postgres::types::PgInterval;
use uuid::Uuid;

use treadmill_rs::api::switchboard::jobs::{
    EnqueueJobResponse, JobInfo, JobListResponse, LogStreamCredentials,
};
use treadmill_rs::api::switchboard::{JobInitSpec, JobRequest};

use crate::audit::feed::{AuditFeedResponse, fetch_events_for_entity};
use crate::audit::model::{Job as AuditJob, Subject as AuditSubject};
use crate::audit::{self, events};
use crate::auth::engine::{self, ImageGroupPermission, JobPermission};
use crate::log_streaming::{self, TokenScope};
use crate::routes::params::IdPath;
use crate::serve::AppState;
use crate::sql::{image, job};

/// Default and maximum page sizes for `GET /jobs`.
const DEFAULT_LIST_LIMIT: u32 = 50;
const MAX_LIST_LIMIT: u32 = 200;

/// Query parameters for `GET /jobs`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ListQuery {
    /// Maximum number of jobs per page. Omitted or out-of-range values fall
    /// back to the server's default and bounds.
    limit: Option<u32>,
    /// Opaque keyset cursor from a previous response's `next_cursor`.
    cursor: Option<String>,
}

/// The keyset position encoded in an opaque list `cursor`: the `(queued_at,
/// job_id)` of the last row of the previous page.
#[derive(Serialize, Deserialize)]
struct JobCursor {
    q: DateTime<Utc>,
    id: Uuid,
}

fn encode_cursor(queued_at: DateTime<Utc>, job_id: Uuid) -> String {
    let json = serde_json::to_vec(&JobCursor {
        q: queued_at,
        id: job_id,
    })
    .expect("JobCursor serializes");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
}

/// Decode an opaque list cursor; `None` on any malformation (yielding a 400 at
/// the call site).
fn decode_cursor(cursor: &str) -> Option<(DateTime<Utc>, Uuid)> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .ok()?;
    let parsed: JobCursor = serde_json::from_slice(&bytes).ok()?;
    Some((parsed.q, parsed.id))
}

/// Axum handler for `GET /jobs` — a keyset-paginated listing of the jobs the
/// caller may read (owned via principals, granted, or all for a global admin),
/// newest first.
pub async fn list(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Query(query): Query<ListQuery>,
) -> Result<Json<JobListResponse>, StatusCode> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .clamp(1, MAX_LIST_LIMIT);

    let after = match query.cursor.as_deref() {
        Some(c) => Some(decode_cursor(c).ok_or(StatusCode::BAD_REQUEST)?),
        None => None,
    };

    // Fetch one extra row to learn whether a further page exists.
    let fetch = i64::from(limit) + 1;
    let mut jobs = job::list_visible(subject.user_id(), after, fetch, state.pool())
        .await
        .map_err(|e| {
            tracing::error!("listing visible jobs: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let next_cursor = if jobs.len() as i64 > i64::from(limit) {
        jobs.truncate(limit as usize);
        jobs.last().map(|j| encode_cursor(j.queued_at, j.job_id))
    } else {
        None
    };

    Ok(Json(JobListResponse { jobs, next_cursor }))
}

/// Read tokens are deliberately short-lived. A NATS bearer JWT is only checked
/// at connect time — an already-established connection is not dropped when the
/// token expires — so a tight TTL bounds a leaked token's exposure without
/// disrupting an in-progress live tail. Clients re-request on reconnect.
const READ_TOKEN_TTL: Duration = Duration::from_secs(5 * 60);

/// Axum handler for the `/jobs/{id}/events` path.
pub async fn list_events(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(IdPath { id: job_id }): Path<IdPath>,
) -> Result<Json<AuditFeedResponse>, StatusCode> {
    fetch_events_for_entity(&state, &subject, "job", job_id)
        .await
        .map(Json)
}

/// Axum handler for `POST /jobs`.
///
/// Enqueues a new job and returns its id. The job is inserted in the `queued`
/// state; the polling scheduler places it onto an eligible host later — there is
/// no synchronous host-match result here.
///
/// Authorization:
///   - **Owner.** `req.owner`, if set, must be the caller itself or a group the
///     caller belongs to (`requested ∈ principals(caller)`), else `403`; absent,
///     the job is owned by the caller.
///   - **Resume/restart.** A `ResumeJob`/`RestartJob` references an existing job;
///     the caller must hold `Manage` on it, else `403`.
///   - **Image group.** An `ImageGroup` job requires `use` on the group (else
///     `403`, existence not leaked) and freezes a concrete generation now; a
///     group with no generation to freeze is a `400`.
///
/// Concrete-image (`Image`) validity and host eligibility are **not** checked
/// here: an unresolvable image finalizes the job as `image_error` at dispatch,
/// and host eligibility is the scheduler's authoritative concern (see the
/// `TODO(authz)` below).
pub async fn enqueue(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Json(mut req): Json<JobRequest>,
) -> Result<(StatusCode, Json<EnqueueJobResponse>), StatusCode> {
    let caller = subject.user_id();

    // Resolve and validate the owner: the caller, or a group it belongs to.
    let owner = match req.owner {
        Some(requested) => {
            let reachable = sqlx::query_scalar!(
                "select exists(select 1 from tml_switchboard.principals($1) p where p.id = $2) as \"ok!\"",
                caller,
                requested,
            )
            .fetch_one(state.pool())
            .await
            .map_err(|e| {
                tracing::error!("checking requested job owner reachability: {e}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
            if !reachable {
                return Err(StatusCode::FORBIDDEN);
            }
            requested
        }
        None => caller,
    };

    // Resuming or restarting exposes the referenced job; require `Manage` on it.
    // (Independent of the owner check above — the requested owner may differ from
    // the referenced job's owner; both gates must pass.)
    if let JobInitSpec::ResumeJob { job_id } | JobInitSpec::RestartJob { job_id } = req.init_spec {
        let authorized =
            engine::can_access_job(state.pool(), caller, job_id, JobPermission::Manage)
                .await
                .map_err(|e| {
                    tracing::error!("checking manage access on referenced job {job_id}: {e}");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;
        if !authorized {
            return Err(StatusCode::FORBIDDEN);
        }
    }

    // An image-group job requires `use` on the group and freezes a concrete
    // generation at enqueue, so the candidate set is reproducible (resolution to
    // a concrete member still happens per-host at dispatch). Resolve and validate
    // it up front rather than deferring to dispatch: a missing group or missing
    // `use` is a 403 (existence is not leaked), and a group with no generation to
    // freeze is a 400.
    if let JobInitSpec::ImageGroup {
        image_group,
        generation,
    } = req.init_spec
    {
        let may_use = engine::can_access_image_group(
            state.pool(),
            caller,
            image_group,
            ImageGroupPermission::Use,
        )
        .await
        .map_err(|e| {
            tracing::error!("checking `use` on image group {image_group}: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        if !may_use {
            return Err(StatusCode::FORBIDDEN);
        }

        let frozen = match generation {
            // An explicitly requested generation must exist (nothing to pin
            // otherwise); the FK would also reject it, but a 400 here is clearer.
            Some(g) => {
                if image::fetch_generation(state.pool(), image_group, g)
                    .await
                    .map_err(|e| {
                        tracing::error!(
                            "looking up generation {g} of image group {image_group}: {e}"
                        );
                        StatusCode::INTERNAL_SERVER_ERROR
                    })?
                    .is_none()
                {
                    return Err(StatusCode::BAD_REQUEST);
                }
                g
            }
            None => image::latest_generation(state.pool(), image_group)
                .await
                .map_err(|e| {
                    tracing::error!(
                        "resolving latest generation of image group {image_group}: {e}"
                    );
                    StatusCode::INTERNAL_SERVER_ERROR
                })?
                .ok_or(StatusCode::BAD_REQUEST)?,
        };

        // Pin the resolved generation so the stored freeze is exactly what we
        // authorized and validated (job::insert would otherwise re-derive it).
        req.init_spec = JobInitSpec::ImageGroup {
            image_group,
            generation: Some(frozen),
        };
    }

    // Resolve the timeout (explicit override, else the deployment default) and
    // reject a non-positive one.
    let timeout = req
        .override_timeout
        .unwrap_or(state.config().service.default_job_timeout);
    if timeout <= chrono::Duration::zero() {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    let job_timeout = PgInterval::try_from(timeout).map_err(|e| {
        tracing::warn!("rejecting job with unrepresentable timeout: {e}");
        StatusCode::UNPROCESSABLE_ENTITY
    })?;

    // TODO(authz): enqueue does not verify the caller may run on *any* host the
    // job could match (ownership / `start` grant on eligible hosts). The
    // scheduler is the authoritative gate; until its `eligible_hosts` predicate
    // restricts by enqueuing principal, a job can be placed on any tag-eligible
    // host regardless of who submitted it.

    // Time-ordered (v7) so the primary-key index inserts with good locality and
    // job ids sort by creation time (see also the `queued_at` listing order).
    let job_id = Uuid::now_v7();
    let parameters = req.parameters.clone();
    let mut txn = state.pool().begin().await.map_err(|e| {
        tracing::error!("opening a transaction to enqueue a job: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    job::insert(
        req,
        job_id,
        subject.token_id(),
        Some(owner),
        job_timeout,
        Utc::now(),
        &mut txn,
    )
    .await
    .map_err(|e| {
        tracing::error!("inserting enqueued job {job_id}: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    job::parameters::insert(job_id, parameters, txn.as_mut())
        .await
        .map_err(|e| {
            tracing::error!("inserting parameters for job {job_id}: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Record the enqueue in the same transaction, so the audit row commits
    // atomically with the job (a rolled-back insert announces nothing).
    audit::emit(
        &mut txn,
        &events::JobEnqueued {
            actor: AuditSubject(caller),
            job: AuditJob(job_id),
        },
    )
    .await
    .map_err(|e| {
        tracing::error!("emitting JobEnqueued for {job_id}: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    txn.commit().await.map_err(|e| {
        tracing::error!("committing enqueued job {job_id}: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok((StatusCode::CREATED, Json(EnqueueJobResponse { job_id })))
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
    Path(IdPath { id: job_id }): Path<IdPath>,
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

/// Axum handler for `DELETE /jobs/{id}` — request termination of a job.
///
/// Gated on the caller's `stop` permission (403 for unauthorized, including a
/// nonexistent job). A still-`queued` job is finalized as `user_terminated`
/// immediately; a dispatched job has its terminate signal recorded and the owning
/// host's worker converges (issues StopJob, then finalizes). Returns `202
/// Accepted` when a termination was initiated, or `204 No Content` when the job
/// was already finalized (idempotent no-op).
pub async fn terminate(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(IdPath { id: job_id }): Path<IdPath>,
) -> Result<StatusCode, StatusCode> {
    let authorized =
        engine::can_access_job(state.pool(), subject.user_id(), job_id, JobPermission::Stop)
            .await
            .map_err(|e| {
                tracing::error!("checking job stop access: {e}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
    if !authorized {
        return Err(StatusCode::FORBIDDEN);
    }

    let mut txn = state.pool().begin().await.map_err(|e| {
        tracing::error!("opening a transaction to terminate job {job_id}: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let outcome = job::request_terminate(job_id, Utc::now(), &mut txn)
        .await
        .map_err(|e| {
            tracing::error!("requesting termination of job {job_id}: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Audit only an actual termination; re-terminating an already-finalized job
    // changed nothing, so it records nothing. Emitted in-transaction so the
    // event commits atomically with the state change (or not at all).
    if outcome != job::TerminateOutcome::AlreadyFinalized {
        audit::emit(
            &mut txn,
            &events::JobTerminated {
                actor: AuditSubject(subject.user_id()),
                job: AuditJob(job_id),
                finalized_immediately: outcome == job::TerminateOutcome::FinalizedNow,
            },
        )
        .await
        .map_err(|e| {
            tracing::error!("emitting JobTerminated for {job_id}: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    }

    txn.commit().await.map_err(|e| {
        tracing::error!("committing termination of job {job_id}: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(match outcome {
        // A termination was initiated: finalized now (queued) or the worker will
        // converge (dispatched).
        job::TerminateOutcome::FinalizedNow | job::TerminateOutcome::SignalRequested => {
            StatusCode::ACCEPTED
        }
        // Already terminal: nothing to do.
        job::TerminateOutcome::AlreadyFinalized => StatusCode::NO_CONTENT,
    })
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
    Path(IdPath { id: job_id }): Path<IdPath>,
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

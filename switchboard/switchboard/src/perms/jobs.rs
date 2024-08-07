//! Privileged actions acting on jobs and the job queue.

use crate::kanban::KanbanError;
use crate::model;
use crate::model::job::SqlExitStatus;
use crate::sched::SchedError;
use crate::server::auth::{
    AuthorizationError, DbPermSource, PermissionQueryExecutor, Privilege, PrivilegedAction,
};
use crate::server::AppState;
use axum::async_trait;
use chrono::{DateTime, Utc};
use std::fmt::Debug;
use treadmill_rs::api::switchboard::{ExitStatus, JobRequest, JobResult, JobStatus};
use uuid::Uuid;
// enqueue_ci_job

#[derive(Debug, Clone)]
pub struct EnqueueCIJobAction;
#[async_trait]
impl PrivilegedAction for EnqueueCIJobAction {
    async fn authorize<'s, PQE: PermissionQueryExecutor + Send>(
        self,
        perm_query_exec: PQE,
    ) -> Result<Privilege<'s, Self>, AuthorizationError> {
        perm_query_exec
            .query("enqueue_ci_job".to_owned())
            .await
            .try_into_privilege(self)
    }
}
#[derive(Debug)]
pub enum EnqueueJobError {
    Database,
    Scheduler(SchedError),
}
pub async fn enqueue_ci_job(
    state: &AppState,
    p: Privilege<'_, EnqueueCIJobAction>,
    job: JobRequest,
    auth_source: DbPermSource,
) -> Result<Uuid, EnqueueJobError> {
    let mut transaction = state.pool().begin().await.map_err(|e| {
        tracing::error!("Failed to create transaction: {e}");
        EnqueueJobError::Database
    })?;
    let job_id = Uuid::new_v4();
    let job_model = model::job::insert(
        job_id,
        &job,
        state.config().api.default_job_timeout,
        p.subject(),
        transaction.as_mut(),
    )
    .await
    .map_err(|e| {
        tracing::error!("failed to add job ({}) to transaction: {e}", job_id);
        EnqueueJobError::Database
    })?;
    model::job::params::insert(job_id, job.parameters.clone(), transaction.as_mut())
        .await
        .map_err(|e| {
            tracing::error!(
                "failed to add job ({}) parameters to transaction: {e}",
                job_id
            );
            EnqueueJobError::Database
        })?;
    let _ = sqlx::query!(
        r#"insert into user_privileges(user_id, permission)
        select user_id, unnest($2::text[]) from api_tokens
        where api_tokens.token_id = $1
    ;"#,
        Uuid::from(p.subject().token_id()),
        &[
            format!("read_job_status:{job_id}"),
            format!("stop_job:{job_id}"),
        ]
    )
    .execute(transaction.as_mut())
    .await
    .map_err(|e| {
        tracing::error!("failed to add read_job_status ({job_id}) grant to transaction: {e}");
        EnqueueJobError::Database
    })?;
    transaction.commit().await.map_err(|e| {
        tracing::error!("failed to commit transaction: {e}");
        EnqueueJobError::Database
    })?;

    state
        .scheduler()
        .enqueue(job_model, job.parameters, auth_source)
        .await
        .map_err(EnqueueJobError::Scheduler)?;

    Ok(job_id)
}

// read_job_status

#[derive(Debug, Clone)]
pub struct JobStatusAction {
    pub job_id: Uuid,
}
#[async_trait]
impl PrivilegedAction for JobStatusAction {
    async fn authorize<'source, PQE: PermissionQueryExecutor + Send>(
        self,
        perm_query_exec: PQE,
    ) -> Result<Privilege<'source, Self>, AuthorizationError> {
        perm_query_exec
            .query(format!("read_job_status:{}", &self.job_id))
            .await
            .try_into_privilege(self)
    }
}
#[derive(Debug)]
pub enum JobStatusError {
    JobNotFound,
    Kanban(KanbanError),
    Database(sqlx::Error),
}
pub async fn read_job_status(
    state: &AppState,
    p: Privilege<'_, JobStatusAction>,
) -> Result<JobStatus, JobStatusError> {
    struct SqlJobResult {
        job_id: Uuid,
        supervisor_id: Option<Uuid>,
        exit_status: SqlExitStatus,
        host_output: Option<serde_json::Value>,
        terminated_at: DateTime<Utc>,
    }
    let maybe_result = sqlx::query_as!(SqlJobResult,
        r#"select job_id, supervisor_id, exit_status as "exit_status: _", host_output, terminated_at from job_results where job_id = $1 limit 1;"#,
        p.action().job_id
    )
    .fetch_optional(state.pool())
    .await
    .map_err(|e| {
        tracing::error!(
            "Failed to check results table for job ({})",
            p.action().job_id
        );
        JobStatusError::Database(e)
    })?;
    if let Some(SqlJobResult {
        job_id,
        supervisor_id,
        exit_status,
        host_output,
        terminated_at,
    }) = maybe_result
    {
        return Ok(JobStatus::Terminated(JobResult {
            job_id,
            supervisor_id,
            exit_status: ExitStatus::from(exit_status),
            host_output,
            terminated_at,
        }));
    }
    state
        .scheduler()
        .kanban()
        .job_status(p.action().job_id)
        .await
        .map_err(JobStatusError::Kanban)
}

#[derive(Debug, Clone)]
pub struct StopJobAction {
    pub job_id: Uuid,
}
#[async_trait]
impl PrivilegedAction for StopJobAction {
    async fn authorize<'source, PQE: PermissionQueryExecutor + Send>(
        self,
        perm_query_exec: PQE,
    ) -> Result<Privilege<'source, Self>, AuthorizationError> {
        perm_query_exec
            .query(format!("stop_job:{}", &self.job_id))
            .await
            .try_into_privilege(self)
    }
}
#[derive(Debug)]
pub enum StopJobError {
    Sched(SchedError),
}
pub async fn stop_job(
    state: &AppState,
    p: Privilege<'_, StopJobAction>,
) -> Result<(), StopJobError> {
    state
        .scheduler()
        .stop_job(p.action().job_id)
        .await
        .map_err(StopJobError::Sched)
}

#[derive(Debug, Clone)]
pub struct AccessQueueAction;
#[async_trait]
impl PrivilegedAction for AccessQueueAction {
    async fn authorize<'source, PQE: PermissionQueryExecutor + Send>(
        self,
        perm_query_exec: PQE,
    ) -> Result<Privilege<'source, Self>, AuthorizationError> {
        perm_query_exec
            .query("access_job_queue".to_owned())
            .await
            .try_into_privilege(self)
    }
}

//! Privileged actions acting on jobs and the job queue.

use crate::model;
use crate::sched::SchedError;
use crate::server::auth::{
    AuthorizationError, DbPermSource, PermissionQueryExecutor, Privilege, PrivilegedAction,
};
use crate::server::AppState;
use axum::async_trait;
use std::fmt::Debug;
use treadmill_rs::api::switchboard::JobRequest;
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
    // let active_job = state
    //     .herd()
    //     .try_start_job(
    //         StartJobMessage::from_job_request_with_id(job_id, job.clone()),
    //         job_model,
    //         p.action().supervisor_id,
    //     )
    //     .await
    //     .map_err(|e| {
    //         tracing::error!(
    //             "Failed to start {job:?} on {}: {e}",
    //             p.action().supervisor_id
    //         );
    //         EnqueueJobError::Herd(e)
    //     })?;
    // state.job_market().insert_active(active_job);

    Ok(job_id)
}

// read_job_status

// #[derive(Debug, Clone)]
// pub struct JobStatusAction {
//     pub job_id: Uuid,
// }
// #[async_trait]
// impl PrivilegedAction for JobStatusAction {
//     async fn authorize<'source, PQE: PermissionQueryExecutor + Send>(
//         self,
//         perm_query_exec: PQE,
//     ) -> Result<Privilege<'source, Self>, AuthorizationError> {
//         perm_query_exec
//             .query(format!("read_job_status:{}", &self.job_id))
//             .await
//             .try_into_privilege(self)
//     }
// }
// #[derive(Debug)]
// pub enum JobStatusError {
//     JobNotFound,
//     Herd(HerdError),
//     Kanban(KanbanError),
// }
// pub async fn read_job_status(
//     state: &AppState,
//     p: Privilege<'_, JobStatusAction>,
// ) -> Result<JobStatus, JobStatusError> {
//     state
//         .job_market()
//         .job_status(p.action().job_id)
//         .map_err(JobStatusError::JobMarket)
// }

// #[derive(Debug, Clone)]
// pub struct StopJobAction {
//     pub job_id: Uuid,
// }
// #[async_trait]
// impl PrivilegedAction for StopJobAction {
//     async fn authorize<'source, PQE: PermissionQueryExecutor + Send>(
//         self,
//         perm_query_exec: PQE,
//     ) -> Result<Privilege<'source, Self>, AuthorizationError> {
//         perm_query_exec
//             .query(format!("stop_job:{}", &self.job_id))
//             .await
//             .try_into_privilege(self)
//     }
// }
// #[derive(Debug)]
// pub enum StopJobError {
//     JobNotFound,
//     Herd(HerdError),
//     JobMarket(KanbanError),
// }
// pub async fn stop_job(
//     state: &AppState,
//     p: Privilege<'_, StopJobAction>,
// ) -> Result<(), StopJobError> {
//     state
//         .job_market()
//         .stop_job(p.action().job_id)
//         .await
//         .map_err(StopJobError::JobMarket)
// }

//! Privileged actions acting on jobs and the job queue.

use crate::model::{Job, JobParameter};
use crate::server::auth::{
    AuthorizationError, PermissionQueryExecutor, Privilege, PrivilegedAction,
};
use crate::server::AppState;
use axum::async_trait;
use std::fmt::Debug;
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

    // TODO: add job to ephemeral queue

    Ok(())
}

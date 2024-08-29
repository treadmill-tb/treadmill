use crate::auth::db::DbAuth;
use crate::auth::Privilege;
use crate::serve::AppState;
use crate::service::herd::HerdError;
use crate::service::ServiceError;
use crate::{impl_simple_perm, perms};
use std::collections::HashMap;
use treadmill_rs::api::switchboard::supervisors::list::{ConnFilter, WorkFilter};
use treadmill_rs::api::switchboard::{supervisors, JobRequest, SupervisorStatus};
use uuid::Uuid;

pub struct ListSupervisors {
    pub filter: supervisors::list::Filter,
}
impl_simple_perm!(ListSupervisors, "list_supervisors");

/// Get the current status of the supervisor set passed in, according to the filter in
/// `list_supervisors`.
pub async fn list_supervisors(
    state: &AppState,
    list_supervisors: Privilege<'_, ListSupervisors>,
    supervisors: Vec<Privilege<'_, AccessSupervisorStatus>>,
) -> HashMap<Uuid, SupervisorStatus> {
    let mut map = HashMap::new();
    let filter = list_supervisors.action().filter;
    for supervisor_perm in supervisors {
        let supervisor_id = supervisor_perm.action().supervisor_id;
        if let Ok(status) = access_supervisor_status(state, supervisor_perm).await {
            match (filter.conn, &status) {
                (
                    Some(ConnFilter::Connected),
                    SupervisorStatus::Disconnected | SupervisorStatus::BusyDisconnected { .. },
                ) => continue,
                (
                    Some(ConnFilter::Disconnected),
                    SupervisorStatus::Busy { .. } | SupervisorStatus::Idle,
                ) => continue,
                _ => (),
            }
            match (filter.work, &status) {
                (
                    Some(WorkFilter::Idle),
                    SupervisorStatus::BusyDisconnected { .. } | SupervisorStatus::Busy { .. },
                ) => continue,
                (
                    Some(WorkFilter::Busy),
                    SupervisorStatus::Idle | SupervisorStatus::Disconnected,
                ) => continue,
                _ => (),
            }
            map.insert(supervisor_id, status);
        }
    }

    map
}

pub struct AccessSupervisorStatus {
    pub supervisor_id: Uuid,
}
impl_simple_perm!(
    AccessSupervisorStatus,
    "access_supervisor_status:{}",
    self,
    self.supervisor_id
);
/// Supervisor is not registered, or internal error occurred.
pub enum AccessSupervisorStatusError {
    InvalidSupervisor,
    Internal,
}
/// Read supervisor status.
pub async fn access_supervisor_status(
    state: &AppState,
    supervisor: Privilege<'_, perms::AccessSupervisorStatus>,
) -> Result<SupervisorStatus, AccessSupervisorStatusError> {
    state
        .service()
        .get_supervisor_status(supervisor.action().supervisor_id)
        .await
        .map_err(|e| match e {
            ServiceError::Herd(HerdError::NotRegistered) => {
                AccessSupervisorStatusError::InvalidSupervisor
            }
            _ => AccessSupervisorStatusError::Internal,
        })
}

pub struct SubmitJob;
impl_simple_perm!(SubmitJob, "submit_job");
pub struct RunJobOnSupervisor {
    pub supervisor_id: Uuid,
}
impl_simple_perm!(
    RunJobOnSupervisor,
    "run_job_on_supervisor:{}",
    self,
    self.supervisor_id
);
pub enum SubmitJobError {
    Internal,
    FailedToMatch,
}
pub async fn submit_job(
    state: &AppState,
    p: Privilege<'_, SubmitJob>,
    job_request: JobRequest,
) -> Result<Uuid, SubmitJobError> {
    match state
        .service()
        .register_job(job_request, p.subject().token_id())
        .await
    {
        Ok(job_id) => Ok(job_id),
        Err(e) => {
            tracing::error!("Failed to submit job: registration error: {e}");
            Err(match e {
                ServiceError::FailedToMatch => SubmitJobError::FailedToMatch,
                _ => SubmitJobError::Internal,
            })
        }
    }
}

use crate::auth::Privilege;
use crate::impl_simple_perm;
use crate::serve::AppState;
use std::collections::HashMap;
use treadmill_rs::api::switchboard::supervisors::list::{ConnFilter, WorkFilter};
use treadmill_rs::api::switchboard::{JobRequest, JobStatus, SupervisorStatus, supervisors};
use uuid::Uuid;

// -- list_supervisors

pub struct ListSupervisors {
    pub filter: supervisors::list::Filter,
}
impl_simple_perm!(ListSupervisors, "list_supervisors");

/// Get the current status of the supervisor set passed in, according to the filter in
/// `list_supervisors`.
pub async fn list_supervisors(
    state: &AppState,
    list_supervisors: Privilege<'_, ListSupervisors>,
    supervisors: Vec<Privilege<'_, ReadSupervisorStatus>>,
) -> HashMap<Uuid, SupervisorStatus> {
    let mut map = HashMap::new();
    let filter = list_supervisors.action().filter;
    for supervisor_perm in supervisors {
        let supervisor_id = supervisor_perm.action().supervisor_id;
        if let Ok(status) = read_supervisor_status(state, supervisor_perm).await {
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

// -- read_supervisor_status

pub struct ReadSupervisorStatus {
    pub supervisor_id: Uuid,
}
impl_simple_perm!(
    ReadSupervisorStatus,
    "read_supervisor_status:{}",
    self,
    self.supervisor_id
);
/// Supervisor is not registered, or internal error occurred.
pub enum ReadSupervisorStatusError {
    InvalidSupervisor,
    Internal,
}
/// Read supervisor status.
pub async fn read_supervisor_status(
    state: &AppState,
    supervisor: Privilege<'_, ReadSupervisorStatus>,
) -> Result<SupervisorStatus, ReadSupervisorStatusError> {
    todo!()

    // state
    //     .service()
    //     .get_supervisor_status(supervisor.action().supervisor_id)
    //     .await
    //     .map_err(|e| match e {
    //         ServiceError::Herd(HerdError::NotRegistered) => {
    //             ReadSupervisorStatusError::InvalidSupervisor
    //         }
    //         _ => ReadSupervisorStatusError::Internal,
    //     })
}

// -- submit_job

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
    SupervisorMatchError,
}
pub async fn submit_job(
    state: &AppState,
    p: Privilege<'_, SubmitJob>,
    job_request: JobRequest,
) -> Result<Uuid, SubmitJobError> {
    todo!()

    // match state
    //     .service()
    //     .register_job(job_request, p.subject().token_id())
    //     .await
    // {
    //     Ok(job_id) => Ok(job_id),
    //     Err(e) => {
    //         tracing::error!("Failed to submit job: registration error: {e}");
    //         Err(match e {
    //             ServiceError::SupervisorMatchError => SubmitJobError::SupervisorMatchError,
    //             _ => SubmitJobError::Internal,
    //         })
    //     }
    // }
}

// -- read_job_status

pub struct ReadJobStatus {
    pub job_id: Uuid,
}
impl_simple_perm!(ReadJobStatus, "read_job_status:{}", self, self.job_id);
pub enum ReadJobStatusError {
    Internal,
    NoSuchJob,
}
pub async fn read_job_status(
    state: &AppState,
    p: Privilege<'_, ReadJobStatus>,
) -> Result<JobStatus, ReadJobStatusError> {
    todo!()

    // // TODO: this should not be JobState; want more detail than this
    // state
    //     .service()
    //     .get_job_status(p.action().job_id)
    //     .await
    //     .map_err(|e| match e {
    //         ServiceError::NoSuchJob => ReadJobStatusError::NoSuchJob,
    //         e => {
    //             tracing::error!("Failed to read most recent job state history: {e}");
    //             ReadJobStatusError::Internal
    //         }
    //     })
}

// -- stop_job

pub struct StopJob {
    pub job_id: Uuid,
}
impl_simple_perm!(StopJob, "stop_job:{}", self, self.job_id);
pub enum StopJobError {
    Internal,
    // TODO
    #[allow(dead_code)]
    NoSuchJob,
}
pub async fn stop_job(state: &AppState, p: Privilege<'_, StopJob>) -> Result<(), StopJobError> {
    todo!()

    // state
    //     .service()
    //     .cancel_job_external(p.action().job_id)
    //     .await
    //     .map_err(|e| {
    //         tracing::error!("Failed to stop job ({}): {e}", p.action().job_id);
    //         StopJobError::Internal
    //     })
}

// -- list_jobs

pub struct ListJobs;
impl_simple_perm!(ListJobs, "list_jobs");
pub async fn list_jobs(
    state: &AppState,
    _: Privilege<'_, ListJobs>,
    jobs: Vec<Privilege<'_, ReadJobStatus>>,
) -> HashMap<Uuid, JobStatus> {
    let mut map = HashMap::new();
    for job_perm in jobs {
        let job_id = job_perm.action().job_id;
        if let Ok(status) = read_job_status(state, job_perm).await {
            map.insert(job_id, status);
        }
    }
    map
}

pub use crate::api::switchboard_supervisor::JobInitializingStage;
pub use crate::api::switchboard_supervisor::RunningJobState;
pub use crate::api::switchboard_supervisor::StartJobMessage;
use crate::api::switchboard_supervisor::{
    JobService, ReportedSupervisorStatus, SupervisorEvent, SupervisorJobEvent,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use tokio::sync::oneshot;
use uuid::Uuid;

#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum JobErrorKind {
    /// The requested job is already running and thus cannot be started again.
    AlreadyRunning,

    /// The requested job is already in the process of being shut down.
    AlreadyTerminating,

    /// A job with this ID was previously running on this supervisor,
    /// but we weren't asked to `resume` it.
    JobAlreadyExists,

    /// Cannot resume this job, either because this functionality is
    /// unsupported or because this particular job cannot be resumed.
    CannotResume,

    /// Job with the specified ID cannot be found.
    JobNotFound,

    /// The job cannot be removed, because it is still executing. It must be
    /// terminated first.
    NotTerminated,

    /// The maximum number of concurrent jobs has been reached.
    MaxConcurrentJobs,

    /// The requested image cannot be found (either its manifest or a
    /// resource stated therein cannot be fetched):
    ImageNotFound,

    /// There is some problem with the image.
    ImageInvalid,

    /// The image is not compatible with, or does not meet the
    /// expectations of this supervisor.
    ImageNotCompatible,

    /// Internal error within the supervisor:
    InternalError,
}

#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct JobError {
    pub error_kind: JobErrorKind,
    pub description: String,
}

/// A command the coordinator issues to a supervisor.
///
/// Connectors translate the messages they receive into these and push them into
/// the supervisor's command channel; the supervisor core owns the loop that
/// drains it. A connector never holds a reference to the supervisor.
///
/// [`CoordCommand::StartJob`] carries no acknowledgement: the supervisor
/// reports a start failure as a [`SupervisorJobEvent::Error`], which is the
/// only error channel for it.
#[derive(Debug)]
pub enum CoordCommand {
    StartJob(StartJobMessage),

    /// Stop the execution of a job. The job's record and resources stay
    /// allocated until [`CoordCommand::RemoveJob`].
    ///
    /// Acknowledged with `Ok(())` on an already-terminated or unknown job.
    TerminateJob {
        job_id: Uuid,
        ack: oneshot::Sender<Result<(), JobError>>,
    },

    /// Free a terminated job's record and the resources it retains.
    ///
    /// Acknowledged with [`JobErrorKind::NotTerminated`] on a job that is still
    /// executing, and with `Ok(())` on an unknown job.
    RemoveJob {
        job_id: Uuid,
        ack: oneshot::Sender<Result<(), JobError>>,
    },

    /// Report the supervisor's status: `Idle` when its job slot is empty, and
    /// `HoldingJob` with the occupant's state while it is not.
    StatusRequest {
        reply: oneshot::Sender<ReportedSupervisorStatus>,
    },
}

/// Connector to a coordinator.
///
/// This interface is implemented by all "connectors" that facilitate
/// interactions between supervisors and coordinators. It allows supervisors to
/// deliver events and issue requests to a coordinator, for instance to report
/// their current status.
#[async_trait]
pub trait SupervisorConnector: std::fmt::Debug + Send + Sync + 'static {
    /// Start the connector's main loop.
    ///
    /// Supervisors are expected to execute this method after performing their
    /// startup initialization. A connector will return with `Ok(())` when it
    /// intends the supervisor to shut down, and with `Err(())` in case an error
    /// occurred communicating with the switchboard. In the latter case,
    /// supervisors may or may not try to reconnect by calling `run()` in the
    /// loop.
    async fn run(&self) -> Result<(), ()>;

    async fn emit(&self, supervisor_event: SupervisorEvent);

    async fn update_job_state(
        &self,
        job_id: Uuid,
        job_state: RunningJobState,
        status_message: Option<String>,
    ) {
        self.emit(SupervisorEvent::JobEvent {
            job_id,
            event: SupervisorJobEvent::StateTransition {
                new_state: job_state,
                status_message,
            },
        })
        .await
    }
    async fn report_job_error(&self, job_id: Uuid, error: JobError) {
        self.emit(SupervisorEvent::JobEvent {
            job_id,
            event: SupervisorJobEvent::Error { error },
        })
        .await
    }
    async fn report_job_network_address(&self, job_id: Uuid, address: IpAddr) {
        self.emit(SupervisorEvent::JobEvent {
            job_id,
            event: SupervisorJobEvent::JobNetworkAddress { address },
        })
        .await
    }
    async fn report_job_service_set(&self, job_id: Uuid, services: Vec<JobService>) {
        self.emit(SupervisorEvent::JobEvent {
            job_id,
            event: SupervisorJobEvent::JobServiceSet { services },
        })
        .await
    }
}

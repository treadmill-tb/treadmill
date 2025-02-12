pub use crate::api::switchboard_supervisor::JobInitializingStage;
pub use crate::api::switchboard_supervisor::RunningJobState;
pub use crate::api::switchboard_supervisor::StartJobMessage;
pub use crate::api::switchboard_supervisor::StopJobMessage;
use crate::api::switchboard_supervisor::{SupervisorEvent, SupervisorJobEvent};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum JobErrorKind {
    /// The requested job is already running and thus cannot be started again.
    AlreadyRunning,

    /// The requested job is already in the process of being shut down.
    AlreadyStopping,

    /// A job with this ID was previously running on this supervisor,
    /// but we weren't asked to `resume` it.
    JobAlreadyExists,

    /// Cannot resume this job, either because this functionality is
    /// unsupported or because this particular job cannot be resumed.
    CannotResume,

    /// Job with the specified ID cannot be found.
    JobNotFound,

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobError {
    pub error_kind: JobErrorKind,
    pub description: String,
}

/// Supervisor interface for coordinator connectors.
///
/// A supervisor interacts with a coordinator through a
/// [_connector_](SupervisorConnector). These connectors expect to be passed an
/// instance of [`Supervisor`] to deliver requests and events.
#[async_trait]
pub trait Supervisor: std::fmt::Debug + Send + Sync + 'static {
    /// Start a new job, based on the parameters supplied in the
    /// `StartJobRequest`.
    ///
    /// This method should avoid blocking on long-running operations that should
    /// be able to be interrupted by other requests (such as stopping a job
    /// during an image download).
    ///
    /// A successful return (`Ok(())`) from this method does not imply that the
    /// job was started successfully, but merely that there is not an error to
    /// return at this point. Even after returning from this method, errors can
    /// be reported through [`SupervisorConnector::report_job_error`].
    ///
    /// Implementations should use [`SupervisorConnector::update_job_state`] to
    /// report on progress while starting or stopping a job, or performing
    /// similar actions.
    async fn start_job(this: &Arc<Self>, request: StartJobMessage) -> Result<(), JobError>;

    /// Stop a running job.
    ///
    /// A successful return (`Ok(())`) from this method does not imply that the
    /// job was stopped successfully, but merely that there is not an error to
    /// return at this point. Even after returning from this method, errors can
    /// be reported through [`SupervisorConnector::report_job_error`].
    ///
    /// Implementations should use [`SupervisorConnector::update_job_state`] to
    /// report on progress while starting or stopping a job, or performing
    /// similar actions.
    async fn stop_job(this: &Arc<Self>, request: StopJobMessage) -> Result<(), JobError>;
}

/// Connector to a coordinator.
///
/// This interface is implemented by all "connectors" that facilitate
/// interactions between supervisors and coordinators. It allows supervisors
/// (implementing the [`Supervisor`] trait) to deliver events and issue requests
/// to a coordinator, for instance to report their current status.
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

    async fn update_event(&self, supervisor_event: SupervisorEvent);

    async fn update_job_state(
        &self,
        job_id: Uuid,
        job_state: RunningJobState,
        status_message: Option<String>,
    ) {
        self.update_event(SupervisorEvent::JobEvent {
            job_id,
            event: SupervisorJobEvent::StateTransition {
                new_state: job_state,
                status_message,
            },
        })
        .await
    }
    async fn report_job_error(&self, job_id: Uuid, error: JobError) {
        self.update_event(SupervisorEvent::JobEvent {
            job_id,
            event: SupervisorJobEvent::Error { error },
        })
        .await
    }
    // TODO: we'll likely want to remove this method from here, and instead have
    // supervisors directly interact with log servers to push events. Or have
    // connectors perform these interactions for them...
    async fn send_job_console_log(&self, job_id: Uuid, console_bytes: Vec<u8>) {
        self.update_event(SupervisorEvent::JobEvent {
            job_id,
            event: SupervisorJobEvent::ConsoleLog { console_bytes },
        })
        .await
    }
}

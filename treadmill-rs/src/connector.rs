use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::api::switchboard_supervisor;
pub use crate::api::switchboard_supervisor::JobStartingStage;
pub use crate::api::switchboard_supervisor::JobState;
pub use crate::api::switchboard_supervisor::StartJobRequest;
pub use crate::api::switchboard_supervisor::StopJobMessage as StopJobRequest;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum JobErrorKind {
    /// The requested job is already running and thus cannot be started again.
    AlreadyRunning,

    /// The requested job is already in the process of being shut down.
    AlreadyStopping,

    /// Job with the specified ID cannot be found.
    JobNotFound,

    /// The maximum number of concurrent jobs has been reached.
    MaxConcurrentJobs,

    /// The requested image cannot be found (either its manifest or a
    /// resource stated therein cannot be fetched):
    ImageNotFound,

    /// The image is not compatible with, or does not meet the
    /// expectations of this supervisor.
    ImageNotCompatible,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobError {
    pub request_id: Option<Uuid>,
    pub error_kind: JobErrorKind,
    pub description: String,
}

/// Supervisor interface for coordinator connectors.
///
/// A supervisor interacts with a coordinator through a
/// [_connector_](SupervisorConnector). These connectors expect to be passed an
/// instance of [`Supervisor`] to deliver requests and events.
#[async_trait]
pub trait Supervisor: Send + Sync + 'static {
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
    async fn start_job(this: &Arc<Self>, request: StartJobRequest) -> Result<(), JobError>;

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
    async fn stop_job(this: &Arc<Self>, request: StopJobRequest) -> Result<(), JobError>;
}

/// Connector to a coordinator.
///
/// This interface is implemented by all "connectors" that facilitate
/// interactions between supervisors and coordinators. It allows supervisors
/// (implementing the [`Supervisor`] trait) to deliver events and issue requests
/// to a coordinator, for instance to report their current status.
#[async_trait]
pub trait SupervisorConnector: Send + Sync + 'static {
    /// Start the connector's main loop.
    ///
    /// Supervisors are expected to execute this method after performing their
    /// startup initialization. A connector will return from this method when it
    /// intends the supervisor to shut down.
    async fn run(&self);

    async fn update_job_state(&self, job_id: Uuid, job_state: switchboard_supervisor::JobState);
    async fn report_job_error(&self, job_id: Uuid, error: JobError);

    // TODO: we'll likely want to remove this method from here, and instead have
    // supervisors directly interact with log servers to push events. Or have
    // connectors perform these interactions for them...
    async fn send_job_console_log(&self, job_id: Uuid, console_bytes: Vec<u8>);
}

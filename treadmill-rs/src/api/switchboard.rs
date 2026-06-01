pub mod hosts;
pub mod jobs;

use crate::api::supervisor_puppet::ParameterValue;
use crate::api::switchboard_supervisor::{
    JobInitializingStage, RestartPolicy, RunningJobState, TaskExitStatus,
};
use crate::image::manifest::ImageId;
use base64::Engine;
use chrono::{DateTime, Utc};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_with::{base64::Base64, serde_as};
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use subtle::{Choice, ConstantTimeEq};
use uuid::Uuid;

pub trait JsonProxiedStatus: Serialize + for<'de> Deserialize<'de> {
    fn status_code(&self) -> StatusCode;
}

#[serde_as]
#[derive(schemars::JsonSchema, Debug, Serialize, Deserialize, Eq, Copy, Clone)]
// Use `serde_with::serde_as` since `serde` by itself doesn't support arrays larger than 32 items,
// and also because `serde_with` has builtin base64-encoding support.
pub struct AuthToken(
    #[serde_as(as = "Base64")]
    #[schemars(with = "String")]
    pub [u8; 128],
);
impl AuthToken {
    pub fn encode_for_http(self) -> String {
        base64::prelude::BASE64_STANDARD.encode(self.0)
    }
}
impl ConstantTimeEq for AuthToken {
    fn ct_eq(&self, other: &Self) -> Choice {
        // IMPORTANT: use ConstantTimeEq to mitigate possible timing attacks:
        // [`subtle::ConstantTimeEq`] is implemented for [u8] so this is sufficient
        ConstantTimeEq::ct_eq(&self.0[..], &other.0[..])
    }
}
impl PartialEq for AuthToken {
    fn eq(&self, other: &Self) -> bool {
        self.ct_eq(other).into()
    }
}

/// Response Body that [`login_handler`] emits.
///
/// Indicates that the user successfully authenticated, and was issued `token`, which inherits the
/// user's credentials, and will expire at `expires_at`.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct LoginResponse {
    pub token: AuthToken,
    pub expires_at: DateTime<Utc>,
}

/// Response body for `/auth/whoami`: the identity of the authenticated subject.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct WhoAmIResponse {
    pub user_id: Uuid,
    pub username: String,
    pub full_name: Option<String>,
}

#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum JobInitSpec {
    /// Resume a previously started job.
    ResumeJob { job_id: Uuid },

    /// Restart a job.
    RestartJob { job_id: Uuid },

    /// Which image to base this job off. If the image is not locally cached
    /// at the host, it will be fetched using its manifest prior to executing
    /// the job.
    ///
    /// Images are content-addressed by the SHA-256 digest of their
    /// manifest.
    Image { image_id: ImageId },
}

#[derive(schemars::JsonSchema, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct JobRequest {
    /// What kind of job this is.
    pub init_spec: JobInitSpec,

    /// The set of initial SSH keys to deploy onto the image.
    ///
    /// The image's configuration of the Treadmill puppet daemon determines
    /// how and whether these keys will be loaded.
    pub ssh_keys: Vec<String>,

    pub restart_policy: RestartPolicy,

    /// A hash map of parameters provided to this job execution. These
    /// parameters are provided to the puppet daemon.
    pub parameters: HashMap<String, ParameterValue>,

    /// The tag configuration.
    ///
    /// FIXME: TO BE SPECIFIED
    pub tag_config: String,

    #[serde(with = "crate::util::chrono::optional_duration")]
    #[schemars(with = "Option<String>")]
    pub override_timeout: Option<chrono::Duration>,
}

/// Why a job terminated.
///
/// This records *why* a job stopped and is orthogonal to the
/// [`TaskExitStatus`] (the success/failure of the user's workload) and to any
/// `exit_message`. Mirrors the `tml_switchboard.termination_reason` DB enum.
#[derive(schemars::JsonSchema, Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationReason {
    /// The job's own workload ended it.
    WorkloadExited,
    /// The job requested its own cancellation.
    WorkloadSelfCanceled,
    /// Externally canceled by a user.
    UserCanceled,
    /// Timed out while still queued.
    QueueTimeout,
    /// Timed out while dispatched/executing.
    ExecutionTimeout,
    /// The job's image was bad or could not be fetched (user fault).
    ImageError,
    /// No host matched the job's tag configuration.
    HostMatchError,
    /// The host failed to start the job.
    HostStartFailure,
    /// The host's supervisor dropped the job (lost on reconnect).
    HostDroppedJob,
    /// The host was unreachable (its supervisor disconnected).
    HostUnreachable,
    /// Resuming a previously started job failed.
    ResumeFailed,
    /// An unexpected internal failure.
    InternalError,
}
impl Display for TerminationReason {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            TerminationReason::WorkloadExited => "workload exited",
            TerminationReason::WorkloadSelfCanceled => "workload requested cancellation",
            TerminationReason::UserCanceled => "canceled by user",
            TerminationReason::QueueTimeout => "timed out in queue",
            TerminationReason::ExecutionTimeout => "timed out while executing",
            TerminationReason::ImageError => "image error",
            TerminationReason::HostMatchError => "failed to match a host",
            TerminationReason::HostStartFailure => "host start failure",
            TerminationReason::HostDroppedJob => "host dropped job",
            TerminationReason::HostUnreachable => "host unreachable",
            TerminationReason::ResumeFailed => "failed to resume job",
            TerminationReason::InternalError => "internal switchboard error",
        };
        f.write_str(s)
    }
}
/// Represents the finalized state of a job.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct JobResult {
    /// The job's ID.
    pub job_id: Uuid,
    /// If the job was dispatched, the ID of the host it was dispatched on.
    /// Otherwise [`None`].
    pub host_id: Option<Uuid>,
    /// Why the job terminated.
    pub termination_reason: TerminationReason,
    /// The semantic result of the user's workload, if it ran and reported one.
    /// Orthogonal to `termination_reason`; may be `None` even on a finalized job.
    pub task_exit_status: Option<TaskExitStatus>,
    /// Optional human-readable message (multi-line markdown acceptable).
    pub exit_message: Option<String>,
    /// Time at which the switchboard processed the termination.
    pub terminated_at: DateTime<Utc>,
}
/// Represents a point in a job's lifecycle.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Scheduled,
    Initializing { stage: JobInitializingStage },
    Ready,
    Terminating,
    Terminated,
}
impl TryFrom<JobState> for RunningJobState {
    type Error = JobState;

    fn try_from(value: JobState) -> Result<Self, Self::Error> {
        match value {
            JobState::Queued => Err(value),
            JobState::Scheduled => Err(value),
            JobState::Initializing { stage } => Ok(Self::Initializing { stage }),
            JobState::Ready => Ok(Self::Ready),
            JobState::Terminating => Ok(Self::Terminating),
            JobState::Terminated => Err(value),
        }
    }
}
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum JobEvent {
    StateTransition {
        state: JobState,
        status_message: Option<String>,
    },
    DeclareWorkloadExitStatus {
        task_exit_status: TaskExitStatus,
        workload_output: Option<String>,
    },
    SetExitStatus {
        termination_reason: TerminationReason,
        task_exit_status: Option<TaskExitStatus>,
        status_message: Option<String>,
    },
    FinalizeResult {
        job_result: JobResult,
    },
}

#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct JobSshEndpoint {
    pub host: String,
    pub port: u16,
}

/// This is exclusively an API type
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct ExtendedJobState {
    #[serde(flatten)]
    pub state: JobState,
    pub dispatched_to_host: Option<Uuid>,
    #[serde(default)] // Backwards-compatibility with clients which do not expect this field
    pub ssh_endpoints: Option<Vec<JobSshEndpoint>>,
    #[serde(default)] // Backwards-compatibility with clients which do not expect this field
    pub ssh_user: Option<String>,
    #[serde(default)] // Backwards-compatibility with clients which do not expect this field
    pub ssh_host_keys: Option<Vec<String>>,
    pub result: Option<JobResult>,
}
/// Represents the status of a job as of some point in time.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct JobStatus {
    pub state: ExtendedJobState,
    pub as_of: DateTime<Utc>,
}

/// The user-visible status of a host as exposed by the switchboard REST API.
///
/// Note the difference with [`switchboard_supervisor`](super::switchboard_supervisor)'s
/// [`ReportedSupervisorStatus`](super::switchboard_supervisor::ReportedSupervisorStatus):
/// that one is concerned primarily with over-the-wire communication, so it does
/// not have 'Disconnected' variants. On the switchboard side, the host can be
/// unreachable because its supervisor's WebSocket is down, so we add those
/// variants here.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status")]
#[serde(rename_all = "snake_case")]
pub enum HostStatus {
    Busy {
        job_id: Uuid,
        job_state: RunningJobState,
    },
    BusyDisconnected {
        job_id: Uuid,
        job_state: RunningJobState,
    },
    Idle,
    Disconnected,
}

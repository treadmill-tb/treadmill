pub mod jobs;
pub mod supervisors;

use crate::api::supervisor_puppet::ParameterValue;
use crate::api::switchboard_supervisor::{
    JobInitializingStage, JobUserExitStatus, RestartPolicy, RunningJobState,
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

/// Request Body that [`login_handler`] expects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    pub user_identifier: String,
    pub password: String,
}

#[serde_as]
#[derive(Debug, Serialize, Deserialize, Eq, Copy, Clone)]
// Use `serde_with::serde_as` since `serde` by itself doesn't support arrays larger than 32 items,
// and also because `serde_with` has builtin base64-encoding support.
pub struct AuthToken(#[serde_as(as = "Base64")] pub [u8; 128]);
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginResponse {
    pub token: AuthToken,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum JobInitSpec {
    /// Resume a previously started job.
    ResumeJob { job_id: Uuid },

    /// Restart a job.
    RestartJob { job_id: Uuid },

    /// Which image to base this job off. If the image is not locally cached
    /// at the supervisor, it will be fetched using its manifest prior to
    /// executing the job.
    ///
    /// Images are content-addressed by the SHA-256 digest of their
    /// manifest.
    Image { image_id: ImageId },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
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
    pub override_timeout: Option<chrono::Duration>,
}

// In accordance with the Job Lifecycle document.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitStatus {
    SupervisorMatchError,
    QueueTimeout,
    InternalSupervisorError,
    SupervisorHostStartFailure,
    SupervisorDroppedJob,
    JobCanceled,
    JobTimeout,
    WorkloadFinishedSuccess,
    WorkloadFinishedError,
    WorkloadFinishedUnknown,
}
impl From<JobUserExitStatus> for ExitStatus {
    fn from(value: JobUserExitStatus) -> Self {
        match value {
            JobUserExitStatus::Success => Self::WorkloadFinishedSuccess,
            JobUserExitStatus::Error => Self::WorkloadFinishedError,
            JobUserExitStatus::Unknown => Self::WorkloadFinishedUnknown,
        }
    }
}
impl Display for ExitStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ExitStatus::SupervisorMatchError => "failed to match",
            ExitStatus::QueueTimeout => "timed out in queue",
            ExitStatus::InternalSupervisorError => "internal supervisor error",
            ExitStatus::SupervisorHostStartFailure => "supervisor/host start failure",
            ExitStatus::SupervisorDroppedJob => "supervisor dropped job",
            ExitStatus::WorkloadFinishedError => "user-defined workload terminated with error",
            ExitStatus::WorkloadFinishedSuccess => "user-defined workload terminated with success",
            ExitStatus::WorkloadFinishedUnknown => {
                "user-defined workload terminated with unknown status"
            }
            ExitStatus::JobCanceled => "canceled",
            ExitStatus::JobTimeout => "job timed out while dispatched",
        };
        f.write_str(s)
    }
}
/// Represents the finalized state of a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResult {
    /// The job's ID.
    pub job_id: Uuid,
    /// If the job was dispatched, the ID of the supervisor it was dispatched on.
    /// Otherwise [`None`].
    pub supervisor_id: Option<Uuid>,
    /// Exit status of the job.
    pub exit_status: ExitStatus,
    /// Optional output.
    pub host_output: Option<String>,
    /// Time at which the switchboard processed the termination.
    pub terminated_at: DateTime<Utc>,
}
/// Represents a point in a job's lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum JobEvent {
    StateTransition {
        state: JobState,
        status_message: Option<String>,
    },
    DeclareWorkloadExitStatus {
        workload_exit_status: JobUserExitStatus,
        workload_output: Option<String>,
    },
    SetExitStatus {
        exit_status: ExitStatus,
        status_message: Option<String>,
    },
    FinalizeResult {
        job_result: JobResult,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSshEndpoint {
    pub host: String,
    pub port: u16,
}

/// This is exclusively an API type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtendedJobState {
    #[serde(flatten)]
    pub state: JobState,
    pub dispatched_to_supervisor: Option<Uuid>,
    #[serde(default)] // Backwards-compatibility with clients which do not expect this field
    pub ssh_endpoints: Option<Vec<JobSshEndpoint>>,
    #[serde(default)] // Backwards-compatibility with clients which do not expect this field
    pub ssh_user: Option<String>,
    #[serde(default)] // Backwards-compatibility with clients which do not expect this field
    pub ssh_host_keys: Option<Vec<String>>,
    pub result: Option<JobResult>,
}
/// Represents the status of a job as of some point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStatus {
    pub state: ExtendedJobState,
    pub as_of: DateTime<Utc>,
}

/// Note the difference with [`switchboard_supervisor`](super::switchboard_supervisor)'s
/// [`SupervisorStatus`](super::switchboard_supervisor::ReportedSupervisorStatus): that one is concerned
/// solely primarily with over-the-wire communication, so it does not have 'Disconnected' variants.
/// However, on the switchboard side, we do need those, hence the differrent set of variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status")]
#[serde(rename_all = "snake_case")]
pub enum SupervisorStatus {
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

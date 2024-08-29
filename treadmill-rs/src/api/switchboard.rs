pub mod jobs;
pub mod supervisors;

use crate::api::supervisor_puppet::ParameterValue;
use crate::api::switchboard_supervisor::RestartPolicy;
use crate::connector::{JobError, JobState};
use crate::image::manifest::ImageId;
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_with::{base64::Base64, serde_as};
use std::collections::HashMap;
use subtle::{Choice, ConstantTimeEq};
use uuid::Uuid;

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

#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitStatus {
    FailedToMatch,
    QueueTimeout,
    HostStartFailure,
    HostTerminatedWithError,
    HostTerminatedWithSuccess,
    HostTerminatedTimeout,
    JobCanceled,
    UnregisteredSupervisor,
    HostDroppedJob,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResult {
    pub job_id: Uuid,
    pub supervisor_id: Option<Uuid>,
    pub exit_status: ExitStatus,
    pub host_output: Option<serde_json::Value>,
    pub terminated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobStatus {
    Active {
        // Not sure yet whether we should include this...
        // on_supervisor_id: Uuid,
        job_state: JobState,
    },
    Error {
        job_error: JobError,
    },
    // Hasn't started yet
    Inactive,
    Terminated(JobResult),
}
impl JobStatus {
    pub fn did_terminate(&self) -> bool {
        match self {
            JobStatus::Active { job_state } => match job_state {
                JobState::Finished { .. } | JobState::Canceled => true,
                _ => false,
            },
            JobStatus::Error { .. } => true,
            JobStatus::Inactive => false,
            JobStatus::Terminated(_) => true,
        }
    }
}

/// Note the difference with [`switchboard_supervisor`](super::switchboard_supervisor)'s
/// [`SupervisorStatus`](super::switchboard_supervisor::ReportedSupervisorStatus): that one is concerned
/// solely primarily with over-the-wire communication, so it does not have 'Disconnected' variants.
/// However, on the switchboard side, we do need those, hence the differrent set of variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status")]
#[serde(rename_all = "snake_case")]
pub enum SupervisorStatus {
    Busy { job_id: Uuid, job_state: JobState },
    BusyDisconnected { job_id: Uuid, job_state: JobState },
    Idle,
    Disconnected,
}

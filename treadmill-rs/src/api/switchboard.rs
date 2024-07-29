use crate::api::supervisor_puppet::ParameterValue;
use crate::api::switchboard_supervisor::{JobInitSpec, RendezvousServerSpec, RestartPolicy};
use crate::connector::{JobError, JobState};
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
pub struct AuthToken(#[serde_as(as = "Base64")] pub [u8; 128]);
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

    /// A set of SSH rendezvous servers to tunnel inbound SSH connections
    /// through. Leave empty to avoid using SSH rendezvouz
    /// servers. Supervisors may not support this, in which case they will
    /// not report back any SSH endpoints reachable through the rendezvous
    /// endpoints listed here:
    pub ssh_rendezvous_servers: Vec<RendezvousServerSpec>,

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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnqueueJobRequest {
    /// Supervisor to enqueue job on.
    pub supervisor_id: Uuid,
    /// Job request.
    pub job_request: JobRequest,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EnqueueJobResponse {
    /// Succeeded. (HTTP 200)
    Ok { job_id: Uuid },
    /// Requested supervisor does not exist. (HTTP 404)
    SupervisorNotFound,
    /// Authorization subject does not have sufficient privileges. (HTTP 401)
    Unauthorized,
    /// Job request is invalid. (HTTP 400)
    Invalid { reason: String },
    /// Internal error. (HTTP 500)
    Internal,
    /// Unable to fulfill request due to lack of available time slots.
    Conflict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum JobStatus {
    Active {
        // Not sure yet whether we should include this...
        // on_supervisor_id: Uuid,
        job_state: JobState,
    },
    Error {
        job_error: JobError,
    },
    Inactive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "job_status")]
pub enum JobStatusResponse {
    Ok { job_status: JobStatus },
    JobNotFound,
    Unauthorized,
    Internal,
}

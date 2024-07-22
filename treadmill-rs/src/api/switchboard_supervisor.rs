//! Types used in the interface between the coordinator and supervisor
//! components.

use crate::connector::JobError;
use crate::image::manifest::ImageId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Debug;
use uuid::Uuid;

/// Challenge-based authentication for switchboard-supervisor websocket connections.
pub mod ws_challenge {
    use serde::{Deserialize, Serialize};
    use uuid::Uuid;

    pub static TREADMILL_WEBSOCKET_PROTOCOL: &str = "treadmillv1";

    pub const NONCE_LEN: usize = 32;

    #[derive(Debug, Copy, Clone, Serialize, Deserialize)]
    pub struct ChallengeRequest {
        pub uuid: Uuid,
    }
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Challenge {
        pub switchboard_nonce: [u8; NONCE_LEN],
    }
    #[derive(Debug, Copy, Clone, Serialize, Deserialize)]
    pub struct ChallengeResponse {
        pub switchboard_nonce_signature: ed25519_dalek::Signature,
    }
    #[derive(Debug, Copy, Clone, Serialize, Deserialize)]
    pub enum ChallengeResult {
        Authenticated,
        Unauthenticated,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "type")]
    #[non_exhaustive]
    pub enum ChallengeMessage {
        ChallengeRequest(ChallengeRequest),
        Challenge(Challenge),
        ChallengeResponse(ChallengeResponse),
        ChallengeResult(ChallengeResult),
    }
}

// -- StartJobRequest ------------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ParameterValue {
    pub value: String,
    pub secret: bool,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct RendezvousServerSpec {
    pub client_id: Uuid,
    pub server_base_url: String,
    pub auth_token: String,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum JobInitSpec {
    /// Whether to resume a previously started job.
    ResumeJob { job_id: Uuid },

    /// Whether to restart a previously attempted job.
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
pub struct RestartPolicy {
    pub remaining_restart_count: usize,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct StartJobMessage {
    /// Unique identifier of the job to be started.
    ///
    /// To restart a previously failed or interrupted job, pass the same ID
    /// in as the old job and set `resume_job = true`. The supervisor may
    /// refuse to start a job with a re-used ID if `resume_job` is
    /// deasserted, and must refuse to start a job when it cannot be resumed
    /// and `resume_job` is asserted.
    pub job_id: Uuid,

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
}

// -- StopJobRequest -------------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct StopJobMessage {
    /// Unique identifier of the job to be stopped:
    pub job_id: Uuid,
}

// -- StatusInfo -----------------------------------------------------------------------------------
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum JobStartingStage {
    /// Generic starting stage, for when no other stage is applicable:
    Starting,

    /// Fetching the specified image:
    FetchingImage,

    /// Acquiring resources, such as the root file system, to launch the
    /// board environment.
    Allocating,

    /// Provisioning the environment, such as making any changes to the base
    /// system according to the user-provided customizations.
    Provisioning,

    /// The container is booting. The next transition should
    /// either be into the `Ready` or `Failed` states.
    Booting,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum JobSessionConnectionInfo {
    #[serde(rename = "direct_ssh")]
    DirectSSH {
        hostname: String,
        port: u16,
        host_key_fingerprints: Vec<String>,
    },
    #[serde(rename = "rendezvous_ssh")]
    RendezvousSSH {
        hostname: String,
        port: u16,
        host_key_fingerprints: Vec<String>,
    },
}
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "state")]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Starting {
        stage: JobStartingStage,
        status_message: Option<String>,
    },
    Ready {
        connection_info: Vec<JobSessionConnectionInfo>,
        status_message: Option<String>,
    },
    Stopping {
        status_message: Option<String>,
    },
    Finished {
        status_message: Option<String>,
    },
    Failed {
        status_message: Option<String>,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum SupervisorStatus {
    OngoingJob { job_id: Uuid, job_state: JobState },
    Idle,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum SupervisorEvent {
    UpdateJobState {
        job_id: Uuid,
        job_state: JobState,
    },
    ReportJobError {
        job_id: Uuid,
        error: JobError,
    },
    SendJobConsoleLog {
        job_id: Uuid,
        console_bytes: Vec<u8>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct Request<T> {
    pub request_id: Uuid,
    pub message: T,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct Response<T> {
    pub response_to_request_id: Uuid,
    pub message: T,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "type", content = "message")]
pub enum Message {
    StartJob(StartJobMessage),

    StopJob(StopJobMessage),

    StatusRequest(Request<()>),
    StatusResponse(Response<SupervisorStatus>),

    SupervisorEvent(SupervisorEvent),
}

#[non_exhaustive]
#[derive(Debug)]
pub enum ResponseMessage {
    StatusResponse(SupervisorStatus),
}

impl Message {
    pub fn request_id(&self) -> Option<Uuid> {
        match self {
            Message::StatusRequest(r) => Some(r.request_id),
            Message::StartJob(_)
            | Message::StopJob(_)
            | Message::StatusResponse(_)
            | Message::SupervisorEvent(_) => None,
        }
    }
    pub fn to_response_message(self) -> Result<Response<ResponseMessage>, Message> {
        match self {
            Message::StatusResponse(Response {
                response_to_request_id,
                message,
            }) => Ok(Response {
                response_to_request_id,
                message: ResponseMessage::StatusResponse(message),
            }),
            x => Err(x),
        }
    }
}

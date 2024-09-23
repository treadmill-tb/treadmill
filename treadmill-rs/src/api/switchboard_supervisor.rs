//! Types used in the interface between the coordinator and supervisor
//! components.

use crate::connector::JobError;
use crate::image::manifest::ImageId;
use crate::util::chrono::duration as human_duration;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

pub mod websocket {
    pub static TREADMILL_WEBSOCKET_PROTOCOL: &str = "treadmillv1";
    pub static TREADMILL_WEBSOCKET_CONFIG: &str = "tml-socket-config";
}

// -- (Configuration)  -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SocketConfig {
    /// PING-PONG keepalive configuration.
    pub keepalive: KeepaliveConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeepaliveConfig {
    /// How often the switchboard should send PING messages to the supervisor.
    #[serde(with = "human_duration")]
    pub ping_interval: chrono::TimeDelta,
    /// Minimum time without a PONG response before the switchboard closes the connection.
    #[serde(with = "human_duration")]
    pub keepalive: chrono::TimeDelta,
}

// -- StartJobRequest ------------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone)]
pub struct ParameterValue {
    pub value: String,
    pub secret: bool,
}

impl std::fmt::Debug for ParameterValue {
    /// Custom implementation of [`std::fmt::Debug`] for [`ParameterValue`] to
    /// avoid leaking secrets in logs:
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        let mut debug_struct = f.debug_struct("ParameterValue");
        debug_struct.field("secret", &self.secret);

        // TODO: Requires nightly feature debug_closure_helpers
        // debug_struct.field_with("value", |f| {
        //     if self.secret {
        //         write!(f, "***")
        //     } else {
        //         <String as std::fmt::Debug>::fmt(&self.value, f)
        //     }
        // });

        // For now, print the secret as if it were a string (with
        // quotation marks) with contents "***":
        debug_struct.field("value", if self.secret { &"***" } else { &self.value });

        debug_struct.finish()
    }
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
pub enum ImageSpecification {
    /// Whether to resume a previously started job.
    ResumeJob { job_id: Uuid },

    /// Which image to base this job off. If the image is not locally cached
    /// at the supervisor, it will be fetched using its manifest prior to
    /// executing the job.
    ///
    /// Images are content-addressed by the SHA-256 digest of their
    /// manifest.
    ///
    /// Note that if a job is being restarted, it will use this variant.
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

    pub image_spec: ImageSpecification,

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

// -- Job/Supervisor Status ------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum JobInitializingStage {
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

    /// The host is booting. The next transition should
    /// either be into the `Ready` or `Failed` states.
    Booting,
}
// #[derive(Serialize, Deserialize, Debug, Clone)]
// #[serde(rename_all = "snake_case")]
// pub enum JobSessionConnectionInfo {
//     #[serde(rename = "direct_ssh")]
//     DirectSSH {
//         hostname: String,
//         port: u16,
//         host_key_fingerprints: Vec<String>,
//     },
//     #[serde(rename = "rendezvous_ssh")]
//     RendezvousSSH {
//         hostname: String,
//         port: u16,
//         host_key_fingerprints: Vec<String>,
//     },
// }
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "state")]
#[serde(rename_all = "snake_case")]
pub enum RunningJobState {
    Initializing { stage: JobInitializingStage },
    Ready,
    // Ready { connection_info: Vec<JobSessionConnectionInfo>, },
    Terminating,
    Terminated,
}
#[derive(Serialize, Deserialize, Debug, Copy, Clone)]
#[serde(rename_all = "snake_case")]
pub enum JobUserExitStatus {
    Success,
    Error,
    Unknown,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum SupervisorJobEvent {
    StateTransition {
        new_state: RunningJobState,
        status_message: Option<String>,
    },
    DeclareExitStatus {
        user_exit_status: JobUserExitStatus,
        host_output: Option<String>,
    },
    // Technically a state transition
    Error {
        error: JobError,
    },
    ConsoleLog {
        console_bytes: Vec<u8>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum ReportedSupervisorStatus {
    OngoingJob {
        job_id: Uuid,
        job_state: RunningJobState,
    },
    Idle,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum SupervisorEvent {
    JobEvent {
        job_id: Uuid,
        event: SupervisorJobEvent,
    },
}

// -- General Request/Response ---------------------------------------------------------------------

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
    StatusResponse(Response<ReportedSupervisorStatus>),

    SupervisorEvent(SupervisorEvent),
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
#[derive(Debug)]
#[non_exhaustive]
pub enum ResponseMessage {
    StatusResponse(ReportedSupervisorStatus),
}

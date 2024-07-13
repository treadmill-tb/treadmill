//! Types used in the interface between the coordinator and supervisor
//! components.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Serialize, Debug, Clone)]
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

#[derive(Serialize, Debug, Clone)]
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

#[derive(Serialize, Debug, Clone)]
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

#[derive(Deserialize, Debug, Clone)]
pub struct ParameterValue {
    pub value: String,
    pub secret: bool,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct RendezvousServerSpec {
    pub client_id: Uuid,
    pub server_base_url: String,
    pub auth_token: String,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct StartJobMessage {
    /// Identifier of this particular request, to associate responses:
    pub request_id: Uuid,

    /// Unique identifier of the job to be started.
    ///
    /// To restart a previously failed or interrupted job, pass the same ID
    /// in as the old job and set `resume_job = true`. The supervisor may
    /// refuse to start a job with a re-used ID if `resume_job` is
    /// deasserted, and must refuse to start a job when it cannot be resumed
    /// and `resume_job` is asserted.
    pub job_id: Uuid,

    /// Whether to resume a previously started job.
    pub resume_job: bool,

    /// Which image to base this job off. If the image is not locally cached
    /// at the supervisor, it will be fetched using its manifest prior to
    /// executing the job.
    ///
    /// Images are content-addressed by the SHA-256 digest of their
    /// manifest.
    pub image_id: [u8; 32],

    /// The set of initial SSH keys to deploy onto the image.
    ///
    /// The image's configuration of the Treadmill puppet daemon determines
    /// how and whether these keys will be loaded.
    pub ssh_keys: Vec<String>,

    /// A set of SSH rendezvous servers to tunnel inbound SSH connections
    /// through. Leave empty to avoid using SSH rendezvouz
    /// servers. Supervisors may not support this, in which case they will
    /// not report back any SSH endpoints reachable through the rendezvous
    /// endpoints listed here:
    pub ssh_rendezvous_servers: Vec<RendezvousServerSpec>,

    /// A hash map of parameters provided to this job execution. These
    /// parameters are provided to the puppet daemon.
    pub parameters: HashMap<String, ParameterValue>,

    /// A command to execute in the target. This command will be provided to
    /// the puppet daemon. How this is executed depends on the puppet
    /// daemon's configuration.
    pub run_command: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct StopJobMessage {
    /// Identifier of this particular request, to associate responses:
    pub request_id: Uuid,

    /// Unique identifier of the job to be stopped:
    pub job_id: Uuid,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "type")]
pub enum SSEMessage {
    RequestStatusUpdate { request_id: Uuid },
    StartJob(StartJobMessage),
    StopJob(StopJobMessage),
}

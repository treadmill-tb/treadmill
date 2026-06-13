pub mod audit;
#[cfg(feature = "client")]
pub mod client;
pub mod images;
pub mod users;

use crate::api::supervisor_puppet::ParameterValue;
use crate::api::switchboard_supervisor::RestartPolicy;
use crate::image::Digest;
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_with::{base64::Base64, serde_as};
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use subtle::{Choice, ConstantTimeEq};
use uuid::Uuid;

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

    /// Base this job off a concrete image registered in the switchboard
    /// catalog, addressed by its OCI manifest digest. At dispatch the
    /// switchboard resolves the digest to its registry locations and hands the
    /// supervisor a content-addressed [`ImageSpecification::Image`].
    ///
    /// [`ImageSpecification::Image`]:
    ///     crate::api::switchboard_supervisor::ImageSpecification::Image
    Image { image: Digest },

    /// Base this job off a registered image *group* (an OCI image index),
    /// addressed by its index digest. After a host is chosen, the switchboard
    /// matcher selects the group member whose required host tags the chosen
    /// host satisfies and dispatches that concrete member's digest.
    ImageGroup { image_group: Digest },
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

    /// Host eligibility: the set of tags the chosen host must carry (as a
    /// superset) for this job to be scheduled onto it. Tags are opaque strings
    /// (`key=value` pairs or bare flags, by convention only), matched by
    /// containment against the host's tags.
    #[serde(default)]
    pub host_tag_requirements: Vec<String>,

    /// Target (DUT) eligibility: an ordered array of requested targets, each a
    /// set of tags an attached DUT must carry (as a superset). The scheduler
    /// assigns each entry to a distinct `host_targets` row on the chosen host.
    /// Empty requests no DUTs. Target tags do not affect image selection.
    #[serde(default)]
    pub target_requirements: Vec<Vec<String>>,

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

pub mod audit;
#[cfg(feature = "client")]
pub mod client;
pub mod hosts;
pub mod images;
pub mod jobs;
pub mod users;

use crate::api::supervisor_puppet::ParameterValue;
use crate::api::switchboard_supervisor::RestartPolicy;
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
    pub [u8; 32],
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

/// Response body for the unauthenticated `/auth/providers` endpoint: which login
/// methods a switchboard offers, so a frontend can render the right buttons
/// without hardcoding provider knowledge.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct AuthProvidersResponse {
    /// Real OAuth providers (e.g. GitHub) the user can start a login flow with.
    pub oauth: Vec<OAuthProviderInfo>,
    /// Built-in mock sign-in identities. Non-empty ONLY when the
    /// development-only mock provider is enabled; each is an unauthenticated,
    /// canned identity. A frontend MUST surface these as development-only.
    pub mock_identities: Vec<MockIdentityInfo>,
}

/// A real OAuth provider advertised by `/auth/providers`.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct OAuthProviderInfo {
    /// Stable provider key, e.g. `"github"`.
    pub name: String,
    /// Human-readable label for a button, e.g. `"GitHub"`.
    pub display_name: String,
    /// Path, relative to the switchboard origin, that starts the login flow,
    /// e.g. `"/api/v1/auth/github/login"`.
    pub login_path: String,
}

/// A built-in mock identity advertised by `/auth/providers` (development only).
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct MockIdentityInfo {
    /// Identity selector passed back to the mock login endpoint.
    pub key: String,
    /// Human-readable label, e.g. `"alice (admin)"`.
    pub label: String,
    /// Path, relative to the switchboard origin, that starts this identity's
    /// login, e.g. `"/api/v1/auth/mock/login?identity=alice"`.
    pub login_path: String,
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
    /// catalog, addressed by its catalog id (`POST /images`). At dispatch the
    /// switchboard resolves the image to its registry locations and hands the
    /// supervisor a content-addressed [`ImageSpecification::Image`].
    ///
    /// [`ImageSpecification::Image`]:
    ///     crate::api::switchboard_supervisor::ImageSpecification::Image
    Image { image: Uuid },

    /// Base this job off a registered image *group*, addressed by its stable id.
    /// `generation` pins a specific membership snapshot; when omitted, the
    /// group's latest generation is resolved and frozen onto the job at enqueue.
    /// After a host is chosen, the switchboard matcher selects the generation's
    /// member whose required host tags the chosen host satisfies and dispatches
    /// that concrete member.
    ImageGroup {
        image_group: Uuid,
        #[serde(default)]
        generation: Option<u32>,
    },
}

/// The execution-lifecycle state of a job, mirroring the
/// `tml_switchboard.job_state` DB enum. This is the switchboard's own view of
/// where a job is in its lifecycle (queued → assigned → executing → finalized),
/// distinct from the supervisor-reported
/// [`RunningJobState`](crate::api::switchboard_supervisor::RunningJobState).
#[derive(schemars::JsonSchema, Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Enqueued, awaiting placement onto a host by the scheduler.
    Queued,
    /// Placed on a host but not yet reported as executing.
    Assigned,
    /// The host is bringing the job up (see
    /// [`JobInitializingStage`](crate::api::switchboard_supervisor::JobInitializingStage)).
    Initializing,
    /// The job is running and ready.
    Ready,
    /// The job is shutting down.
    Terminating,
    /// Terminal: the job has ended (see `termination_reason`).
    Finalized,
}

#[derive(schemars::JsonSchema, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct JobRequest {
    /// What kind of job this is.
    pub init_spec: JobInitSpec,

    /// The subject (user or group) to own the enqueued job. Must be the caller
    /// itself or a group the caller is a member of; absent, ownership defaults
    /// to the caller. Ownership decides who can later read, stop, and manage the
    /// job (see `job_grants`).
    #[serde(default)]
    pub owner: Option<Uuid>,

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
    /// superset) for this job to be assigned to it. Tags are opaque strings
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
    /// The job's workload terminated (e.g., QEMU VM shutdown).
    WorkloadExited,
    /// The job requested its own termination (e.g., by requesting termination
    /// through the puppet).
    WorkloadSelfTerminated,
    /// Externally terminated by a user.
    UserTerminated,
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
            TerminationReason::WorkloadSelfTerminated => "workload requested termination",
            TerminationReason::UserTerminated => "terminated by user",
            TerminationReason::QueueTimeout => "timed out in queue",
            TerminationReason::ExecutionTimeout => "timed out while executing",
            TerminationReason::ImageError => "image error",
            TerminationReason::HostMatchError => "failed to match a host",
            TerminationReason::HostStartFailure => "host start failure",
            TerminationReason::HostDroppedJob => "host dropped job",
            TerminationReason::HostUnreachable => "host unreachable",
            TerminationReason::ResumeFailed => "failed to resume job",
            TerminationReason::InternalError => "internal error",
        };
        f.write_str(s)
    }
}

//! Job-scoped client API types.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::switchboard::{JobState, TerminationReason};
use crate::api::switchboard_supervisor::{JobInitializingStage, RestartPolicy, TaskExitStatus};
use crate::image::Digest;

/// Connection credentials for tailing/replaying a job's console logs over NATS,
/// returned by `POST /jobs/{id}/log-token`.
///
/// The token is a short-lived **bearer** user JWT scoped to *subscribe* to this
/// job's log subjects (`subject`); the client connects to `nats_url` with the
/// token string alone (no nkey seed). The token only needs to be valid at
/// connect time — an established NATS connection is not dropped when the JWT
/// expires — so a client re-requests credentials when it next reconnects, after
/// roughly `expires_in_secs`.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct LogStreamCredentials {
    /// NATS client URL to connect to (e.g. `nats://nats.example:4222`).
    pub nats_url: String,
    /// Subject wildcard covering all of this job's log channels:
    /// `logs.<job-id>.>`.
    pub subject: String,
    /// Bearer user JWT authorizing subscribe on `subject`.
    pub token: String,
    /// Seconds until the token's `exp`; re-request credentials after this
    /// elapses (only needed to open a *new* connection).
    pub expires_in_secs: u64,
}

/// What a job is based off, as seen by `GET /jobs/{id}`. Collapses the four
/// mutually-exclusive DB columns (`image_digest` / `image_group_digest` /
/// `resume_job_id` / `restart_job_id`) into a single tagged variant; the
/// separately-recorded concrete dispatch digest is reported alongside as
/// [`JobInfo::resolved_image_digest`].
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobImageRef {
    /// Based off a concrete catalog image, addressed by its OCI manifest digest.
    Image { digest: Digest },
    /// Based off a registered image *group*, addressed by its index digest; the
    /// concrete member is chosen at dispatch.
    ImageGroup { digest: Digest },
    /// Resumes a previously started job.
    Resume { job_id: Uuid },
    /// Restarts a previously started job (inherits its image reference).
    Restart { job_id: Uuid },
}

/// One job parameter as exposed by `GET /jobs/{id}`. A parameter flagged
/// `secret` is **redacted**: its `value` is `None` and only the key + flag are
/// visible. Non-secret parameters carry their plaintext `value`.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct JobParameterView {
    /// Whether this parameter was submitted as secret.
    pub secret: bool,
    /// The plaintext value, or `None` when `secret` (withheld).
    pub value: Option<String>,
}

/// An SSH endpoint a running job can be reached on.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct SshEndpoint {
    pub ssh_host: String,
    pub ssh_port: u16,
}

/// The full server-side view of a single job, returned by `GET /jobs/{id}`.
///
/// Covers the job's identity, ownership, lifecycle state, the spec it was
/// enqueued with, and — once it has run — its placement and terminal outcome.
/// Secret parameters are redacted (see [`JobParameterView`]).
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct JobInfo {
    pub job_id: Uuid,
    /// Owning subject (user or group); `None` if the owner was deleted
    /// (orphaned).
    pub owner_id: Option<Uuid>,

    /// Where the job is in its lifecycle.
    pub state: JobState,
    /// The sub-stage while `state` is [`JobState::Initializing`]; `None`
    /// otherwise.
    pub initializing_stage: Option<JobInitializingStage>,

    /// What the job is based off.
    pub image: JobImageRef,
    /// The concrete manifest digest actually dispatched, recorded at dispatch;
    /// `None` until then.
    pub resolved_image_digest: Option<Digest>,

    pub ssh_keys: Vec<String>,
    pub restart_policy: RestartPolicy,
    /// Host eligibility tags this job requires (superset match against a host's
    /// tags).
    pub host_tag_requirements: Vec<String>,
    /// Target (DUT) eligibility: one tag set per requested target, in submission
    /// order.
    pub target_requirements: Vec<Vec<String>>,
    /// Job parameters, keyed by name; secret values are redacted.
    pub parameters: HashMap<String, JobParameterView>,
    /// How long the job may run before it is killed, in seconds.
    pub timeout_secs: i64,

    /// When the job was enqueued.
    pub queued_at: DateTime<Utc>,
    /// When the job was dispatched onto a host; `None` if not yet started.
    pub started_at: Option<DateTime<Utc>>,
    /// The host the job is (or was) dispatched on; `None` if unplaced.
    pub dispatched_on_host_id: Option<Uuid>,
    /// SSH endpoints the running job is reachable on; `None` until reported.
    pub ssh_endpoints: Option<Vec<SshEndpoint>>,

    /// Why the job terminated; `None` until finalized.
    pub termination_reason: Option<TerminationReason>,
    /// The user workload's success/failure outcome, orthogonal to
    /// `termination_reason`; `None` if never reported.
    pub task_exit_status: Option<TaskExitStatus>,
    /// A human-readable detail accompanying termination, if any.
    pub exit_message: Option<String>,
    /// When the job was finalized; `None` until then.
    pub terminated_at: Option<DateTime<Utc>>,

    /// When the job row was last updated.
    pub last_updated_at: DateTime<Utc>,
}

/// Response body of `POST /jobs`: the id assigned to the freshly enqueued job.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct EnqueueJobResponse {
    pub job_id: Uuid,
}

/// A compact per-job row for the `GET /jobs` listing — identity, ownership,
/// lifecycle state, and the key timestamps/outcome, without the heavier
/// per-job detail (parameters, target requirements, ssh keys) that
/// [`JobInfo`] carries. Fetch the full view with `GET /jobs/{id}`.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct JobSummary {
    pub job_id: Uuid,
    /// Owning subject (user or group); `None` if orphaned.
    pub owner_id: Option<Uuid>,
    pub state: JobState,
    pub image: JobImageRef,
    pub queued_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub terminated_at: Option<DateTime<Utc>>,
    /// The host the job is (or was) dispatched on; `None` if unplaced.
    pub dispatched_on_host_id: Option<Uuid>,
    pub termination_reason: Option<TerminationReason>,
    pub task_exit_status: Option<TaskExitStatus>,
}

/// Response body of `GET /jobs`: a page of jobs the caller can read, newest
/// first.
///
/// Pagination is **keyset** on `(queued_at, job_id)` descending: when
/// `next_cursor` is `Some`, pass it back as the `cursor` query parameter to
/// fetch the next page; `None` means the last page. There is no total count.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct JobListResponse {
    pub jobs: Vec<JobSummary>,
    /// Opaque cursor for the next page, or `None` on the last page.
    pub next_cursor: Option<String>,
}

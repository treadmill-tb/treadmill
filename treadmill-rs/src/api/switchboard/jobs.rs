//! Job-scoped client API types.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::switchboard::{JobState, TerminationReason};
use crate::image::Digest;

/// The fine-grained stage of a job that is still coming up, exposed as
/// `initializing_stage` on [`JobInfo`] while its `state` is `initializing` (and
/// null in every other state). A job advances through these stages in order as
/// the host fetches its image, allocates resources, provisions the environment,
/// and boots, before it becomes `ready`.
#[derive(schemars::JsonSchema, Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobInitializingStage {
    /// Generic starting stage, reported before any more specific stage applies.
    Starting,
    /// Fetching the job's image.
    FetchingImage,
    /// Acquiring resources (such as the root filesystem) for the environment.
    Allocating,
    /// Applying the requested customizations to the base system.
    Provisioning,
    /// The host is booting; the job becomes `ready` once it is up.
    Booting,
}

/// The user workload's success/failure outcome, exposed as `task_exit_status`
/// on [`JobInfo`]/[`JobSummary`].
///
/// It is orthogonal to *why* the job terminated (see [`TerminationReason`]): the
/// outcome reflects the workload's own result, reported by the host while the
/// job runs and revisable until it ends. `pending` means the workload was still
/// running when last reported; a null `task_exit_status` means no outcome was
/// ever reported.
#[derive(schemars::JsonSchema, Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskExitStatus {
    /// The workload is running; its result is not yet determined.
    Pending,
    /// The workload completed successfully.
    Success,
    /// The workload failed.
    Failure,
}

/// How many times a job may be **automatically restarted** after it is dropped
/// by its host, supplied with the job at enqueue (`POST /jobs`).
///
/// Each automatic restart enqueues a successor that inherits one fewer; the
/// count a running job still has left is reported back as [`RestartPolicyState`]
/// on [`JobInfo`]. A `max_restarts` of `0` disables automatic restarts.
#[derive(schemars::JsonSchema, Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RestartPolicy {
    /// Maximum number of automatic restarts to grant this job.
    pub max_restarts: u32,
}

/// The automatic-restart budget a job still has left, exposed as
/// `restart_policy` on [`JobInfo`].
///
/// A job enqueued with a [`RestartPolicy`] starts at its `max_restarts`; each
/// automatic restart spends one, so this reports how many restarts the job (or
/// the successor currently standing in for it) still has remaining.
#[derive(schemars::JsonSchema, Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RestartPolicyState {
    /// How many automatic restarts this job still has remaining.
    pub remaining_restarts: u32,
}

/// One parameter supplied with a job at enqueue (`POST /jobs`), passed through
/// to the puppet daemon running the workload.
///
/// Flag a parameter `secret` to have its value withheld wherever the job is
/// later read back (it surfaces as a redacted [`JobParameterView`]); non-secret
/// parameters are returned verbatim.
#[derive(schemars::JsonSchema, Serialize, Deserialize, Clone)]
pub struct JobParameter {
    /// The parameter's value.
    pub value: String,
    /// Whether to treat the value as secret (withheld when the job is read
    /// back).
    pub secret: bool,
}

impl std::fmt::Debug for JobParameter {
    /// Custom [`std::fmt::Debug`] that never prints a secret parameter's value,
    /// so a debug-formatted job request cannot leak it into logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_struct("JobParameter")
            .field("secret", &self.secret)
            .field("value", if self.secret { &"***" } else { &self.value })
            .finish()
    }
}

/// Connection credentials for tailing/replaying a job's console logs over NATS,
/// returned by `POST /jobs/{id}/nats-log-token`.
///
/// The token is a short-lived **bearer** user JWT scoped to this job only: it
/// may *subscribe* to the job's log subjects (`subject`) and its own inboxes
/// (`inbox_prefix`), and *publish* to the slice of the JetStream API needed to
/// run an ordered consumer against the job's stream (`stream`) — enough to
/// replay stored history and then follow live. The client connects with the
/// token string alone (no nkey seed) to whichever endpoint suits its transport:
/// `websocket_url` for browsers, `nats_url` for native TCP clients. The same
/// token authorizes either. The token only needs to be valid at connect time —
/// an established NATS connection is not dropped when the JWT expires — so a
/// client re-requests credentials when it next reconnects, after roughly
/// `expires_in_secs`.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct NatsLogStreamCredentials {
    /// Plain-TCP NATS client URL (e.g. `nats://nats.example:4222`), for native
    /// clients that speak the binary protocol. Nullable: a deployment may
    /// choose to expose only the WebSocket endpoint publicly, in which case
    /// only `websocket_url` is returned. Browsers cannot use this — they must
    /// use `websocket_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nats_url: Option<String>,
    /// NATS **WebSocket** URL (e.g. `wss://nats.example:443`), for browser
    /// clients, which cannot speak the plain TCP protocol. Absent when the
    /// deployment does not expose a WebSocket listener; a browser client cannot
    /// stream logs against such a deployment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub websocket_url: Option<String>,
    /// Subject wildcard covering all of this job's log channels:
    /// `logs.<job-id>.>`.
    pub subject: String,
    /// JetStream stream holding this job's logs: `logs-<job-id>`.
    pub stream: String,
    /// Inbox prefix the client **must** configure on its connection
    /// (`_INBOX.logs-<job-id>`): the token's subscribe permission covers only
    /// inboxes under this prefix, not the account-default `_INBOX.>`.
    pub inbox_prefix: String,
    /// The server's JetStream domain, when it is configured with one; the
    /// client must address the JetStream API through it. Usually absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jetstream_domain: Option<String>,
    /// Bearer user JWT authorizing the scope described above.
    pub token: String,
    /// Seconds until the token's `exp`; re-request credentials after this
    /// elapses (only needed to open a *new* connection).
    pub expires_in_secs: u64,
}

/// What a job is based off, as seen by `GET /jobs/{id}`: a concrete image, an
/// image group (with the frozen generation), or a resume/restart of an earlier
/// job. The concrete manifest digest actually dispatched is reported separately
/// as `resolved_image_digest`.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobImageRef {
    /// Based off a concrete catalog image, addressed by its catalog id.
    Image { image_id: Uuid },
    /// Based off a registered image *group*, addressed by its id plus the frozen
    /// generation; the concrete member is chosen at dispatch.
    ImageGroup { group_id: Uuid, generation: u32 },
    /// Resumes a previously started job.
    Resume { job_id: Uuid },
    /// Restarts a previously started job (inherits its image reference).
    Restart { job_id: Uuid },
}

/// One job parameter as exposed by `GET /jobs/{id}`. A parameter flagged
/// `secret` is **redacted**: its `value` is null and only the key and the
/// `secret` flag are visible. Non-secret parameters carry their plaintext
/// `value`.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct JobParameterView {
    /// Whether this parameter was submitted as secret.
    pub secret: bool,
    /// The plaintext value, or null when the parameter is secret (withheld).
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
    /// Owning subject (user or group); null if the owner was deleted
    /// (orphaned).
    pub owner_id: Option<Uuid>,

    /// Where the job is in its lifecycle.
    pub state: JobState,
    /// The sub-stage while `state` is `initializing`; null otherwise.
    pub initializing_stage: Option<JobInitializingStage>,

    /// What the job is based off.
    pub image: JobImageRef,
    /// The concrete manifest digest actually dispatched, recorded at dispatch;
    /// null until then.
    pub resolved_image_digest: Option<Digest>,

    pub ssh_keys: Vec<String>,
    pub restart_policy: RestartPolicyState,
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
    /// When the job was dispatched onto a host; null if not yet started.
    pub started_at: Option<DateTime<Utc>>,
    /// The host the job is (or was) dispatched on; null if unplaced.
    pub dispatched_on_host_id: Option<Uuid>,
    /// SSH endpoints the running job is reachable on; null until reported.
    pub ssh_endpoints: Option<Vec<SshEndpoint>>,

    /// Why the job terminated; null until finalized.
    pub termination_reason: Option<TerminationReason>,
    /// The user workload's success/failure outcome, orthogonal to
    /// `termination_reason`; null if never reported.
    pub task_exit_status: Option<TaskExitStatus>,
    /// A human-readable detail accompanying termination, if any.
    pub exit_message: Option<String>,
    /// When the job was finalized; null until then.
    pub terminated_at: Option<DateTime<Utc>>,
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
    /// Owning subject (user or group); null if orphaned.
    pub owner_id: Option<Uuid>,
    pub state: JobState,
    pub image: JobImageRef,
    pub queued_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub terminated_at: Option<DateTime<Utc>>,
    /// The host the job is (or was) dispatched on; null if unplaced.
    pub dispatched_on_host_id: Option<Uuid>,
    pub termination_reason: Option<TerminationReason>,
    pub task_exit_status: Option<TaskExitStatus>,
}

/// Response body of `GET /jobs`: a page of jobs the caller can read, newest
/// first.
///
/// Pagination is **keyset** on `(queued_at, job_id)` descending: when
/// `next_cursor` is non-null, pass it back as the `cursor` query parameter to
/// fetch the next page; a null `next_cursor` means the last page. There is no
/// total count.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct JobListResponse {
    pub jobs: Vec<JobSummary>,
    /// Opaque cursor for the next page, or null on the last page.
    pub next_cursor: Option<String>,
}

//! Job-scoped client API types.

use std::collections::HashMap;
use std::net::IpAddr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::switchboard::hosts::SpecDocument;
use crate::api::switchboard::{JobState, TerminationReason};
use crate::host_spec::HostSpec;
use crate::image::Digest;
use crate::util::Secret;

/// A permission on a job. `permissions` on [`JobInfo`] reports which of these
/// the viewer holds (an owner or global admin holds all of them).
#[derive(schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobPermission {
    /// May read the job (its info, listing, and console logs).
    Read,
    /// May request the job's termination.
    Stop,
    /// May perform privileged operations on the job, such as resuming or
    /// restarting it (owner holds this implicitly).
    Manage,
}

/// What happens when a job's lease expires.
#[derive(schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobLeaseExpiryAction {
    /// Hard deadline: the job is stopped (`execution_timeout`).
    Terminate,
    /// The job keeps running unprotected and becomes reclaimable: the scheduler
    /// may stop it (`preempted`) to place a job with no idle host available.
    /// Absent that demand it runs on past its lease.
    Preempt,
}

/// A requested change to a job's lease, carried by `PATCH /jobs/{id}`.
///
/// Wire form is a string in one of three shapes, each naming a different anchor:
///
/// | form | meaning |
/// |---|---|
/// | `"30m"` | set the lease to this length, measured from the job's start |
/// | `"+30m"` | lengthen by this much; `-` shortens, floored at zero |
/// | `"2026-08-28T14:00:00Z"` | end the lease at this instant (started jobs only) |
///
/// Note that `"+30m"` *compounds*: repeated on a timer it grows the lease
/// without bound. A keepalive should send the absolute form instead, recomputed
/// each tick, which is idempotent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseSpec {
    /// Set the lease length outright.
    Set(chrono::TimeDelta),
    /// Lengthen (or, when negative, shorten) the lease.
    Adjust(chrono::TimeDelta),
    /// End the lease at an absolute instant.
    ExpiresAt(DateTime<Utc>),
}

impl std::fmt::Display for LeaseSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseSpec::Set(d) => write!(f, "{}", fundu::Duration::from(*d)),
            LeaseSpec::Adjust(d) => {
                let sign = if *d < chrono::TimeDelta::zero() {
                    '-'
                } else {
                    '+'
                };
                write!(f, "{sign}{}", fundu::Duration::from(d.abs()))
            }
            LeaseSpec::ExpiresAt(t) => write!(f, "{}", t.to_rfc3339()),
        }
    }
}

impl std::str::FromStr for LeaseSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        fn duration(s: &str) -> Result<chrono::TimeDelta, String> {
            fundu::DurationParser::new()
                .parse(s)
                .map_err(|e| e.to_string())?
                .try_into()
                .map_err(|_| format!("duration out of range: {s}"))
        }

        if let Some(rest) = s.strip_prefix('+') {
            return Ok(LeaseSpec::Adjust(duration(rest)?));
        }
        if let Some(rest) = s.strip_prefix('-') {
            return Ok(LeaseSpec::Adjust(-duration(rest)?));
        }
        if let Ok(t) = DateTime::parse_from_rfc3339(s) {
            return Ok(LeaseSpec::ExpiresAt(t.with_timezone(&Utc)));
        }
        duration(s).map(LeaseSpec::Set)
    }
}

impl Serialize for LeaseSpec {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for LeaseSpec {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl schemars::JsonSchema for LeaseSpec {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "LeaseSpec".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let mut schema = String::json_schema(generator);
        schema.insert(
            "description".into(),
            "A lease change: `\"30m\"` (set), `\"+30m\"` / `\"-10m\"` (adjust), or an RFC 3339 timestamp (absolute expiry).".into(),
        );
        schema
    }
}

/// Why a requested lease change was refused (`409 Conflict` on
/// `PATCH /jobs/{id}`). The whole request is rejected, so a label change sent
/// alongside a refused lease change is not applied either.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct LeaseRejection {
    pub code: LeaseRejectionCode,
    /// Human-readable explanation; not intended to be parsed.
    pub message: String,
    /// The latest expiry that would have been granted, when one exists.
    pub max_lease_expires_at: Option<DateTime<Utc>>,
    /// When asking again could plausibly succeed; null if never.
    pub retry_after_secs: Option<i64>,
}

/// The machine-readable discriminant of a [`LeaseRejection`]. Clients must
/// tolerate unknown values.
#[derive(schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseRejectionCode {
    /// The job is finalized, or a stop has already been signalled for it.
    JobTerminating,
    /// An absolute lease end was requested for a job that has not started, so
    /// there is no clock to measure it against.
    NotStarted,
    /// A deployment policy caps the lease below what was requested.
    PolicyLimit,
    /// The resources cannot be held for as long as requested.
    ResourcePressure,
}

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

/// One parameter supplied with a job at enqueue (`POST /jobs`), which the job
/// reads from `GET /jobs/{id}/environment`.
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
    pub token: Secret<String>,
    /// Seconds until the token's `exp`; re-request credentials after this
    /// elapses (only needed to open a *new* connection).
    pub expires_in_secs: u64,
}

/// Connection credentials for sending typed input to a job's serial console
/// over NATS, returned by `POST /jobs/{id}/nats-console-input-token`.
///
/// The token is a short-lived **bearer** user JWT that may *publish* to this
/// job's console-input subject (`subject`) — nothing else. Every publish is
/// recorded server-side, so treat the channel as monitored; feedback (echo)
/// arrives through the separate log-streaming (read) channel. As with
/// [`NatsLogStreamCredentials`], the token gates connect time only: an
/// established connection outlives its expiry, and a client re-requests
/// credentials when it next reconnects, after roughly `expires_in_secs`.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct NatsConsoleInputCredentials {
    /// Plain-TCP NATS client URL (e.g. `nats://nats.example:4222`), for native
    /// clients that speak the binary protocol. Nullable: a deployment may
    /// choose to expose only the WebSocket endpoint publicly, in which case
    /// only `websocket_url` is returned. Browsers cannot use this — they must
    /// use `websocket_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nats_url: Option<String>,
    /// NATS **WebSocket** URL (e.g. `wss://nats.example:443`), for browser
    /// clients, which cannot speak the plain TCP protocol. Absent when the
    /// deployment does not expose a WebSocket listener; a browser client
    /// cannot send console input against such a deployment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub websocket_url: Option<String>,
    /// The subject to publish typed input to: `console-in.<job-id>`.
    pub subject: String,
    /// Bearer user JWT authorizing publish to `subject`.
    pub token: Secret<String>,
    /// Seconds until the token's `exp`; re-request credentials after this
    /// elapses (only needed to open a *new* connection).
    pub expires_in_secs: u64,
}

/// An endpoint under which a job's announced service can be reached.
#[derive(schemars::JsonSchema, Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct JobServiceEndpoint {
    /// The gateway hostname the service is published under.
    ///
    /// Already contains the job-part (i.e., `<service>-<job-id>.<gw-fqdn>`).
    pub hostname: String,
    /// The port under which the gateway can be reached at `domain`.
    pub port: u16,
}

/// Access credentials for one of a job's announced services, returned by
/// `POST /jobs/{id}/services/{service}/token`.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct JobServiceCredentials {
    /// Every gateway hostname + port the service is published under, primary
    /// first. The gateway hostname already contains the job-part (i.e.,
    /// `<service>-<job-id>.<gw-fqdn>`).
    ///
    /// The same token is accepted at each, so a client may substitute
    /// the host of `url` for any of these.
    pub endpoints: Vec<JobServiceEndpoint>,
    /// The signed token admitting the caller to this one service.
    pub token: Secret<String>,
    /// When the token stops being accepted. Minting again yields a fresh one;
    /// an already-minted token is not invalidated before this by anything,
    /// including the job ending or the caller's access being revoked.
    pub expires_at: DateTime<Utc>,
}

/// The body of `PUT /jobs/{id}/exit-status`, by which a job reports its own
/// outcome. It may do so any number of times while it runs, each report
/// replacing the last.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct JobExitStatusRequest {
    /// The workload's outcome.
    pub outcome: TaskExitStatus,
    /// An optional human-readable note, recorded as the job's `exit_message`.
    /// Null clears it.
    pub message: Option<String>,
}

/// One service a job announces in `PUT /jobs/{id}/services`, which carries the
/// job's complete set.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobServiceAnnouncement {
    /// Identifies the service within its job: 1 to 16 lowercase alphanumeric
    /// characters, starting with a letter.
    pub name: String,
    /// Optional human-readable text to display.
    pub label: Option<String>,
    /// A token the client interprets to decide how to connect (`webapp`,
    /// `sshws`, …).
    pub protocol: String,
}

/// A gateway under which a job's services are published.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobGatewayEndpoint {
    /// The gateway's base domain. A service is reachable at
    /// `<service>-<job-id>.<base_domain>`.
    pub base_domain: String,
    /// The port the gateway listens on.
    pub port: u16,
}

/// What a job needs to validate the service tokens its gateways admit, so that
/// reaching a service takes a valid token at the gateway and at the job.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobGatewayInfo {
    /// The `iss` every service token carries.
    pub issuer: String,
    /// The switchboard's public key for verifying service tokens, PEM-encoded.
    pub signing_public_key: String,
    /// Identifier of `signing_public_key`, carried as a token's `kid`.
    pub key_id: String,
    /// The gateways the job's services are published under.
    pub endpoints: Vec<JobGatewayEndpoint>,
}

/// Everything a running job needs to set itself up, returned by
/// `GET /jobs/{id}/environment` to the job's own token.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct JobEnvironment {
    /// The host the job was dispatched to.
    pub host_id: Uuid,
    /// The host's current spec, normalized to the latest version, as a document
    /// conforming to the schema at `GET /hosts/spec-schema`. Null for a host
    /// that has never been described.
    #[schemars(with = "Option<SpecDocument>")]
    pub host_spec: Option<HostSpec>,
    /// Gateway material, or null when this deployment runs without gateways.
    pub gateway: Option<JobGatewayInfo>,
    /// The job's parameters, secret values included.
    pub parameters: HashMap<String, JobParameter>,
}

/// The image a job references: a concrete image, or an image set with its
/// frozen generation. Resumed and restarted jobs carry their predecessor's.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobImageReference {
    /// A concrete catalog image, addressed by its manifest digest.
    Image { manifest_digest: Digest },
    /// An image set and its frozen generation; the member is chosen at
    /// dispatch.
    ImageSet { set_id: Uuid, generation: u32 },
}

/// A job's image: what it references, and the concrete image it runs.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobImage {
    pub reference: JobImageReference,
    /// The concrete image: chosen at dispatch, or for a resume the image its
    /// predecessor ran. Null until known.
    pub resolved_digest: Option<Digest>,
}

/// The job a resumed or restarted job continues.
#[derive(schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobPredecessor {
    /// Continues from the predecessor's working directory on its host.
    Resume { job_id: Uuid },
    /// Starts afresh from the predecessor's image reference.
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

/// One service a running job announces, as exposed by `GET /jobs/{id}`.
///
/// All three fields are opaque to the switchboard, which stores and echoes them
/// without interpretation. `name` identifies the service within its job,
/// `label` is optional human-readable text to display, and `protocol` is a
/// token the client interprets to decide how to connect (`webapp`, `sshws`, …).
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct JobServiceView {
    pub name: String,
    pub label: Option<String>,
    pub protocol: String,
}

/// The full server-side view of a single job, returned by `GET /jobs/{id}`.
///
/// Covers the job's identity, ownership, lifecycle state, the spec it was
/// enqueued with, and — once it has run — its placement and terminal outcome.
/// Secret parameters are redacted (see [`JobParameterView`]).
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct JobInfo {
    pub job_id: Uuid,
    /// The user-provided display label, if any.
    pub label: Option<String>,
    /// Owning subject (user or group); null if the owner was deleted
    /// (orphaned).
    pub owner_id: Option<Uuid>,

    /// Where the job is in its lifecycle.
    pub state: JobState,
    /// The sub-stage while `state` is `initializing`; null otherwise.
    pub initializing_stage: Option<JobInitializingStage>,

    pub image: JobImage,
    /// The job this one resumes or restarts, if any.
    pub predecessor: Option<JobPredecessor>,

    pub restart_policy: RestartPolicyState,
    /// Host eligibility tags this job requires (superset match against a host's
    /// tags).
    /// The CEL expression this job's host had to satisfy, as submitted.
    pub host_cel_predicate: String,
    /// Job parameters, keyed by name; secret values are redacted.
    pub parameters: HashMap<String, JobParameterView>,
    /// The job's protected window, in seconds, measured from `started_at`.
    pub lease_duration_secs: i64,
    /// When the lease expires (`started_at + lease_duration`); null until the
    /// job starts.
    pub lease_expires_at: Option<DateTime<Utc>>,
    /// What happens when the lease expires.
    pub lease_expiry_action: JobLeaseExpiryAction,

    /// When the job was enqueued.
    pub queued_at: DateTime<Utc>,
    /// When the job was dispatched onto a host; null if not yet started.
    pub started_at: Option<DateTime<Utc>>,
    /// The host the job is (or was) dispatched on; null if unplaced.
    pub dispatched_on_host_id: Option<Uuid>,

    /// Why the job terminated; null until finalized.
    pub termination_reason: Option<TerminationReason>,
    /// The user workload's success/failure outcome, orthogonal to
    /// `termination_reason`; null if never reported.
    pub task_exit_status: Option<TaskExitStatus>,
    /// The job's own human-readable note on its outcome, if any.
    pub exit_message: Option<String>,
    /// The supervisor's description of the error that ended the job, if one
    /// did.
    pub job_error: Option<String>,
    /// When the job was finalized; null until then.
    pub terminated_at: Option<DateTime<Utc>>,

    /// The set of currently announced services by the job.
    pub services: Vec<JobServiceView>,
    /// The job's internal IP address (generally not publicly routable).
    pub job_ip_address: Option<IpAddr>,

    /// The viewer's permissions on this job.
    pub permissions: Vec<JobPermission>,
}

/// Response body of `POST /jobs`: the id assigned to the freshly enqueued job.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct EnqueueJobResponse {
    pub job_id: Uuid,
    /// How the job's host requirements met the fleet at submission. A job with
    /// `schedulable == 0` was still accepted — the fleet may change — but will
    /// sit queued until it does, which is otherwise indistinguishable from
    /// waiting for a busy host.
    pub host_requirements: crate::api::switchboard::hosts::HostRequirementsReport,
}

/// Defaults applied to a new job (`GET /jobs/defaults`).
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct JobDefaults {
    /// The lease duration used when `JobRequest::lease_duration` is absent, in
    /// seconds.
    pub lease_duration_secs: i64,
}

/// A patch to a job (`PATCH /jobs/{id}`). Only the fields listed here are
/// mutable; a request carrying any other field is rejected. Omitting a field
/// leaves it unchanged; sending an explicit `null` clears it.
#[derive(schemars::JsonSchema, Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateJobRequest {
    /// The job's display label: 1 to 256 characters of ASCII letters, digits,
    /// spaces and `()_,.#-`, not starting or ending with a space, and not
    /// unique.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_with::rust::double_option"
    )]
    #[schemars(with = "Option<String>")]
    pub label: Option<Option<String>>,

    /// A change to the job's lease. May be refused; see [`LeaseRejection`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<LeaseSpec>,

    /// What should happen when the lease expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expiry_action: Option<JobLeaseExpiryAction>,
}

/// A compact per-job row for the `GET /jobs` listing — identity, ownership,
/// lifecycle state, and the key timestamps/outcome, without the heavier per-job
/// detail (parameters, target requirements) that [`JobInfo`] carries. Fetch the
/// full view with `GET /jobs/{id}`.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct JobSummary {
    pub job_id: Uuid,
    /// The user-provided display label, if any.
    pub label: Option<String>,
    /// Owning subject (user or group); null if orphaned.
    pub owner_id: Option<Uuid>,
    pub state: JobState,
    pub image: JobImage,
    pub predecessor: Option<JobPredecessor>,
    pub queued_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub terminated_at: Option<DateTime<Utc>>,
    /// The host the job is (or was) dispatched on; null if unplaced.
    pub dispatched_on_host_id: Option<Uuid>,
    pub termination_reason: Option<TerminationReason>,
    pub task_exit_status: Option<TaskExitStatus>,
    /// When the lease expires; null until the job starts.
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub lease_expiry_action: JobLeaseExpiryAction,
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    fn parse(s: &str) -> LeaseSpec {
        s.parse().unwrap()
    }

    #[test]
    fn lease_spec_forms_parse_by_their_anchor() {
        assert_eq!(parse("30m"), LeaseSpec::Set(TimeDelta::minutes(30)));
        assert_eq!(parse("+30m"), LeaseSpec::Adjust(TimeDelta::minutes(30)));
        assert_eq!(parse("-10m"), LeaseSpec::Adjust(TimeDelta::minutes(-10)));
        assert_eq!(
            parse("2026-08-28T14:00:00Z"),
            LeaseSpec::ExpiresAt("2026-08-28T14:00:00Z".parse().unwrap())
        );
    }

    #[test]
    fn lease_spec_rejects_garbage() {
        assert!("".parse::<LeaseSpec>().is_err());
        assert!("later".parse::<LeaseSpec>().is_err());
        assert!("+".parse::<LeaseSpec>().is_err());
    }

    #[test]
    fn lease_spec_round_trips_through_json() {
        for s in ["30m", "+30m", "-10m", "2026-08-28T14:00:00Z"] {
            let spec = parse(s);
            let json = serde_json::to_string(&spec).unwrap();
            assert_eq!(serde_json::from_str::<LeaseSpec>(&json).unwrap(), spec);
        }
    }
}

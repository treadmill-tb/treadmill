//! Types used in the interface between the switchboard and supervisor
//! components.
//!
//! # Protocol versioning & evolution policy
//!
//! The protocol carries a two-level version. The **major** rides the WebSocket
//! subprotocol token ([`websocket::TREADMILL_WEBSOCKET_PROTOCOL`], e.g.
//! `treadmillv1`): standard subprotocol negotiation selects a common token, and
//! an incompatible peer fails the HTTP upgrade cleanly. The **minor** (plus an
//! optional set of feature flags) rides the handshake: the supervisor advertises
//! its minor in the [`websocket::TREADMILL_PROTOCOL_MINOR_HEADER`] request
//! header, and the switchboard answers with a [`ServerHello`] in the
//! [`websocket::TREADMILL_WEBSOCKET_CONFIG`] response header. The
//! **effective minor** for the connection is `min(client, server)`.
//!
//! To keep additive change non-breaking, all changes to the wire types MUST
//! follow these rules:
//!
//! 1. Protocol types never use `#[serde(deny_unknown_fields)]`, so an older
//!    receiver silently ignores fields a newer peer adds.
//! 2. New fields are additive only: `Option<T>` or `#[serde(default)]`.
//! 3. Adding a message variant requires a **minor** bump, and the variant must
//!    not be emitted below the negotiated effective minor — older peers cannot
//!    deserialize an unknown tag.
//! 4. Removing or renaming a field, changing a field's type, or changing a tag
//!    is **breaking** and requires a **major** bump (a new subprotocol token).
//!
//! The committed JSON Schema snapshots (see `treadmill-rs/protocol-schema/`)
//! make each change classifiable as additive (minor) or breaking (major).

use crate::connector::JobError;
use crate::image::Digest;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use uuid::Uuid;

pub mod websocket {
    /// WebSocket subprotocol token carrying the protocol **major** version.
    /// The trailing integer MUST equal [`super::PROTOCOL_MAJOR`].
    pub static TREADMILL_WEBSOCKET_PROTOCOL: &str = "treadmillv1";
    /// Response header (switchboard → supervisor) carrying the JSON-encoded
    /// [`super::ServerHello`].
    pub static TREADMILL_WEBSOCKET_CONFIG: &str = "tml-socket-config";
    /// Request header (supervisor → switchboard) carrying the supervisor's
    /// advertised protocol **minor** version as a decimal integer.
    pub static TREADMILL_PROTOCOL_MINOR_HEADER: &str = "tml-protocol-minor";
}

// -- Protocol version & handshake -----------------------------------------------------------------

/// The protocol major version implemented by this build. Must match the integer
/// in [`websocket::TREADMILL_WEBSOCKET_PROTOCOL`].
pub const PROTOCOL_MAJOR: u16 = 1;
/// The protocol minor version implemented by this build. Bumped (additively)
/// whenever a new message variant or feature flag is introduced.
///
/// minor 1: `StartJobMessage.log_streaming` (NATS/JetStream log streaming).
pub const PROTOCOL_MINOR: u16 = 1;

/// A two-level protocol version. `major` is also pinned by the WebSocket
/// subprotocol token; `minor` is negotiated in the handshake.
#[derive(schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    /// The version implemented by this build.
    pub const CURRENT: ProtocolVersion = ProtocolVersion {
        major: PROTOCOL_MAJOR,
        minor: PROTOCOL_MINOR,
    };
}

/// The switchboard's handshake reply, serialized into the
/// [`websocket::TREADMILL_WEBSOCKET_CONFIG`] response header.
///
/// Replaces the former empty `SocketConfig`: it now carries the switchboard's
/// protocol version and the set of optional feature flags it supports. Per the
/// evolution policy, unknown `features` entries are ignored by older peers, so
/// this set may grow without a major bump.
///
/// Keepalive intervals are deliberately **not** part of the handshake: each side
/// runs its own keepalive with local config.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct ServerHello {
    pub protocol: ProtocolVersion,
    #[serde(default)]
    pub features: BTreeSet<String>,
}

// -- StartJobRequest ------------------------------------------------------------------------------

#[derive(schemars::JsonSchema, Serialize, Deserialize, Clone)]
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

/// One registry location an image can be pulled from.
///
/// An image's *identity* is its `manifest_digest`; a location is a
/// `(registry, repository)` that serves those bytes. The supervisor protocol
/// carries an ordered list of locations so a supervisor can fail over across
/// them — every location serves the same digest, so any that succeeds is
/// interchangeable (see `doc/oci-image-migration-plan.md` §D12/§D16).
#[derive(schemars::JsonSchema, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct ImageLocation {
    /// Registry authority (`host:port`) the bytes can be pulled from.
    pub registry: String,
    /// Repository path within the registry, e.g. `treadmill/ubuntu-22.04`.
    pub repository: String,
}

#[derive(schemars::JsonSchema, Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum ImageSpecification {
    /// Whether to resume a previously started job.
    ResumeJob { job_id: Uuid },

    /// Which image to base this job off. If the image is not locally cached
    /// at the supervisor, it will be fetched (from one of `locations`) prior
    /// to executing the job.
    ///
    /// The supervisor protocol is always **content-addressed**: an image is
    /// identified by the OCI `manifest_digest` of its manifest, and
    /// `locations` is an ordered, failover list of the registry locations that
    /// serve it. Switchboard resolves human labels/sets to a concrete digest
    /// + locations before dispatch (Phase 4).
    ///
    /// Note that if a job is being restarted, it will use this variant.
    Image {
        manifest_digest: Digest,
        locations: Vec<ImageLocation>,
    },
}
#[derive(schemars::JsonSchema, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct RestartPolicy {
    pub remaining_restart_count: usize,
}
#[derive(schemars::JsonSchema, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct StartJobMessage {
    /// Unique identifier of the job to be started.
    ///
    /// Resuming a previously started job is requested via
    /// [`ImageSpecification::ResumeJob`] in `image_spec` (not via this id):
    /// for a resume, `image_spec` carries the original job's id. The
    /// supervisor must refuse to start a job it cannot resume when a resume
    /// is requested.
    pub job_id: Uuid,

    pub image_spec: ImageSpecification,

    /// The set of initial SSH keys to deploy onto the image.
    ///
    /// The image's configuration of the Treadmill puppet daemon determines
    /// how and whether these keys will be loaded.
    pub ssh_keys: Vec<String>,

    pub restart_policy: RestartPolicy,

    /// A hash map of parameters provided to this job execution. These
    /// parameters are provided to the puppet daemon.
    pub parameters: HashMap<String, ParameterValue>,

    /// Per-job log-streaming destination, or `None` when the deployment runs
    /// with log streaming disabled. The supervisor captures console output and
    /// publishes it to the NATS server described here (see
    /// [`LogStreamingDispatch`]). Additive and optional: older supervisors that
    /// do not understand this field simply ignore it and capture nothing.
    #[serde(default)]
    pub log_streaming: Option<LogStreamingDispatch>,
}

/// A supervisor console-output channel, used as the final token of a log
/// stream's NATS subject (`logs.<job-id>.<channel>`).
#[derive(schemars::JsonSchema, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum LogChannel {
    /// qemu's standard output.
    QemuStdout,
    /// qemu's standard error.
    QemuStderr,
    /// The workload's serial console (qemu routes it to a `-chardev socket`).
    Serial,
}

impl LogChannel {
    /// The subject token for this channel — the final element of the NATS
    /// subject `logs.<job-id>.<channel>`. Equal to the serde representation.
    pub fn as_subject_token(self) -> &'static str {
        match self {
            LogChannel::QemuStdout => "qemu-stdout",
            LogChannel::QemuStderr => "qemu-stderr",
            LogChannel::Serial => "serial",
        }
    }
}

/// Per-job log-streaming destination handed to a supervisor in
/// [`StartJobMessage`].
///
/// The supervisor publishes captured console output to the NATS server at
/// `nats_url`, under subjects `<subject_prefix>.<channel>` (where
/// `subject_prefix` is `logs.<job-id>` and `<channel>` is a [`LogChannel`]
/// token), authenticating with `write_token`.
///
/// `write_token` is a **bearer** user JWT the switchboard mints per dispatch,
/// scoped to publish only this job's subjects; the supervisor connects with the
/// token string alone (no nkey seed is ever shipped).
#[derive(schemars::JsonSchema, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct LogStreamingDispatch {
    /// NATS client URL to connect to (e.g. `nats://nats.example:4222`).
    pub nats_url: String,
    /// Subject prefix for this job's streams: `logs.<job-id>`. A channel token
    /// is appended as `<subject_prefix>.<channel>` (see [`LogChannel`]).
    pub subject_prefix: String,
    /// Bearer user JWT authorizing publish to this job's subjects.
    pub write_token: String,
}

// -- StopJobRequest -------------------------------------------------------------------------------

#[derive(schemars::JsonSchema, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct StopJobMessage {
    /// Unique identifier of the job to be stopped:
    pub job_id: Uuid,
}

// -- Job/Supervisor Status ------------------------------------------------------------------------

#[derive(schemars::JsonSchema, Serialize, Deserialize, Debug, Clone)]
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

    /// The host is booting. The next transition is normally into
    /// [`RunningJobState::Ready`]; a failure instead surfaces as a
    /// [`SupervisorJobEvent::Error`].
    Booting,
}

/// The physical execution state of a job, as reported by the **supervisor**.
///
/// The supervisor is ground truth for what is *physically executing*. The
/// switchboard mirrors this into the `tml_switchboard.job_state` DB column and
/// adopts it verbatim during reconciliation (case 4: the supervisor reports
/// `OngoingJob(J_sb)` for the assigned job, so the DB takes the reported state).
///
/// # Mapping to the DB `job_state`
///
/// The DB `job_state` enum (`switchboard/SCHEMA.sql`) is a superset of this one:
/// it adds the switchboard-owned bookends `queued` and `assigned` (before a
/// supervisor reports anything) and `finalized` (after termination). The
/// assigned, supervisor-owned sub-states map one-to-one:
///
/// | `RunningJobState`     | DB `job_state` |
/// |-----------------------|----------------|
/// | `Initializing{stage}` | `initializing` (+ `initializing_stage`) |
/// | `Ready`               | `ready`        |
/// | `Terminating`         | `terminating`  |
/// | `Terminated`          | `finalized` (see below) |
///
/// `Terminated` is **not** a 1:1 mapping: it folds into the terminal DB record
/// `finalized`, which additionally requires a [`switchboard::TerminationReason`].
/// A supervisor reporting `Terminated` always means a *workload-driven* exit, so
/// the switchboard finalizes it with `termination_reason = workload_exited`; the
/// reason is implied by the report, not encoded in the variant, which is why
/// there is deliberately no total `RunningJobState → job_state` conversion. The
/// workload's [`TaskExitStatus`] is **not** carried here — it is reported
/// out-of-band via [`SupervisorJobEvent::DeclareExitStatus`] whenever the
/// supervisor knows it, so by the time a job terminates the switchboard has
/// already recorded the outcome.
///
/// # The retained-terminal contract
///
/// A supervisor keeps reporting `Terminated` for a finished job — retaining it
/// in memory as `ReportedSupervisorStatus::OngoingJob` — until the switchboard
/// acknowledges it with [`SwitchboardToSupervisor::StopJob`], at which point the
/// supervisor drops the record and becomes `Idle`. This prevents a job that
/// completes while the switchboard is disconnected from being misread, on
/// reconnect, as a dropped job (which would spuriously trigger its restart
/// policy). The record is in-memory only: if the supervisor loses it (restart),
/// it reports `Idle` and the job is legitimately classified as
/// `supervisor_dropped_job`. See the worker's `reconcile` Rustdoc (case 4).
///
/// [`switchboard::TerminationReason`]: crate::api::switchboard::TerminationReason
#[derive(schemars::JsonSchema, Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "state")]
#[serde(rename_all = "snake_case")]
pub enum RunningJobState {
    /// Starting up; `stage` mirrors the DB `initializing_stage`.
    Initializing { stage: JobInitializingStage },
    /// Up and running (the workload is executing).
    Ready,
    // Ready { connection_info: Vec<JobSessionConnectionInfo>, },
    /// Shutting down; the next report is normally `Terminated`.
    Terminating,
    /// The workload has exited. Drives the switchboard's `→ finalized`
    /// transition (`termination_reason = workload_exited`). The workload's
    /// outcome is *not* carried here — it is reported out-of-band via
    /// [`SupervisorJobEvent::DeclareExitStatus`]. The supervisor retains this
    /// report until the switchboard acks it with `StopJob` (see the type-level
    /// Rustdoc).
    Terminated,
}
/// The supervisor's report of how the user's workload is/has turned out — its
/// *task outcome*.
///
/// The supervisor posts this at any point while it is assigned the job (see
/// [`SupervisorJobEvent::DeclareExitStatus`]), independently of termination, and
/// may revise it: `Pending` while the result is not yet known, then `Success` or
/// `Failure`. Once set it is never cleared (it can only move between these three
/// values). It is orthogonal to *why* a job terminated (see
/// [`switchboard::TerminationReason`]) — a job may carry any outcome regardless
/// of its termination reason, or none at all if the supervisor never reported.
///
/// [`switchboard::TerminationReason`]: crate::api::switchboard::TerminationReason
#[derive(schemars::JsonSchema, Serialize, Deserialize, Debug, Copy, Clone)]
#[serde(rename_all = "snake_case")]
pub enum TaskExitStatus {
    /// The workload is running; its result is not yet determined.
    Pending,
    /// The workload completed successfully.
    Success,
    /// The workload failed.
    Failure,
}
/// An asynchronous event a supervisor emits about the job it is executing,
/// wrapped in a [`SupervisorEvent::JobEvent`]. The switchboard mirrors these
/// into the DB out-of-band of reconciliation; reconciliation itself is driven by
/// the [`ReportedSupervisorStatus`] snapshot, not the event stream.
#[derive(schemars::JsonSchema, Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum SupervisorJobEvent {
    /// The job advanced to a new [`RunningJobState`]. The switchboard mirrors
    /// `new_state` into the DB `job_state` column (and a `Terminated` here drives
    /// the `→ finalized` transition; see [`RunningJobState`]). `status_message`
    /// is optional free text for the audit log.
    StateTransition {
        new_state: RunningJobState,
        status_message: Option<String>,
    },
    /// The supervisor sets (or revises) the job's *task outcome*. This is the
    /// dedicated channel for the outcome and is independent of job state: the
    /// supervisor may send it at any point while it is assigned the job, as many
    /// times as it likes, each one overriding the last. `outcome` is required
    /// (`pending`/`success`/`failure`); the outcome can never be cleared back to
    /// unset. `message` is an optional human-readable note recorded as the job's
    /// `exit_message`; each event replaces it, so passing `None` clears it.
    DeclareExitStatus {
        outcome: TaskExitStatus,
        message: Option<String>,
    },
    // Technically a state transition
    /// A job-level error. Semantically a transition toward termination; the
    /// switchboard finalizes the job with an appropriate
    /// [`switchboard::TerminationReason`].
    ///
    /// [`switchboard::TerminationReason`]: crate::api::switchboard::TerminationReason
    Error { error: JobError },
}

/// A supervisor's point-in-time status snapshot, returned in the
/// [`Response`] to a [`SwitchboardToSupervisor::StatusRequest`]. This is the
/// supervisor-side ground truth (`J_sup`) the worker reconciles against the
/// switchboard's assigned job (`J_sb = supervisors.current_job`) on (re)connect.
///
/// The five reconciliation cases (documented in full as Rustdoc on the worker's
/// `reconcile` function) hinge on this value:
/// - `Idle` with no `J_sb` ⇒ aligned;
/// - `Idle` with a `J_sb` ⇒ the job was lost (finalize as `SupervisorDroppedJob`);
/// - `OngoingJob{job_id: J_sb}` ⇒ adopt `job_state` into the DB (case 4); a
///   `Terminated` state finalizes the job and is acked with `StopJob`;
/// - `OngoingJob` of any other (or unassigned) id ⇒ a zombie to `StopJob`.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum ReportedSupervisorStatus {
    /// The supervisor is executing `job_id`, currently in `job_state`.
    OngoingJob {
        job_id: Uuid,
        job_state: RunningJobState,
    },
    /// The supervisor is executing no job.
    Idle,
}
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum SupervisorEvent {
    JobEvent {
        job_id: Uuid,
        event: SupervisorJobEvent,
    },
}

// -- General Request/Response ---------------------------------------------------------------------

/// A request carrying a unique `request_id` for correlation with its reply.
///
/// The reply is a [`Response`] whose `response_to_request_id` echoes this
/// `request_id`. The requester matches replies by this id and may run several
/// requests concurrently. Reconciliation issues exactly one
/// [`SwitchboardToSupervisor::StatusRequest`] on (re)connect and awaits the
/// correlated [`SupervisorToSwitchboard::StatusResponse`]; a missing/timed-out
/// reply is treated as a dead peer.
#[derive(schemars::JsonSchema, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct Request<T> {
    pub request_id: Uuid,
    pub message: T,
}
/// The reply to a [`Request`]. `response_to_request_id` MUST echo the
/// originating request's `request_id` so the requester can correlate it.
#[derive(schemars::JsonSchema, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct Response<T> {
    pub response_to_request_id: Uuid,
    pub message: T,
}
// -- Protocol errors ------------------------------------------------------------------------------

/// A fatal protocol-level error, carried in-band in **both** directions purely
/// for diagnostics (better logs on the receiving side).
///
/// # Contract
///
/// There is no in-band *recovery* protocol. Critical state transitions are
/// idempotent and a reconnect re-synchronises state, so error handling is
/// simply **log → close → reconnect**. On a fatal violation the offended side
/// SHOULD:
///
/// 1. send this [`ProtocolError`] (so the peer can log a precise reason),
/// 2. send a WebSocket `Close` frame with the matching
///    [`ProtocolErrorCode::close_code`], then
/// 3. terminate the connection.
///
/// The peer reconnects and reconciliation re-establishes a consistent state.
#[derive(schemars::JsonSchema, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct ProtocolError {
    pub code: ProtocolErrorCode,
    pub detail: String,
}

/// Enumerated protocol error conditions. Each maps to a WebSocket close code in
/// the RFC 6455 private range (4000–4999) via [`ProtocolErrorCode::close_code`].
#[derive(schemars::JsonSchema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolErrorCode {
    /// A malformed or unexpected message was received (close code 4000).
    ProtocolViolation,
    /// No common protocol major/minor could be negotiated (close code 4001).
    UnsupportedVersion,
    /// An unexpected failure occurred on the sending side (close code 4002).
    InternalError,
    /// The switchboard replaced this connection with a newer one (close code
    /// 4003). Optional; emitted by the switchboard only.
    Superseded,
}

impl ProtocolErrorCode {
    /// The RFC 6455 private-range WebSocket close code for this error.
    pub const fn close_code(self) -> u16 {
        match self {
            ProtocolErrorCode::ProtocolViolation => 4000,
            ProtocolErrorCode::UnsupportedVersion => 4001,
            ProtocolErrorCode::InternalError => 4002,
            ProtocolErrorCode::Superseded => 4003,
        }
    }
}

// -- Directional protocol messages ----------------------------------------------------------------

/// A message sent **from the switchboard to a supervisor**.
///
/// The switchboard is the commanding side of the protocol: it instructs the
/// supervisor to start/stop jobs and polls its status. Modelling the direction
/// in the type means an illegal direction (e.g. the switchboard "reporting" a
/// [`SupervisorEvent`]) is unrepresentable rather than an `unimplemented!()`
/// arm at the dispatch site.
///
/// Wire format: internally tagged with `type`, payload under `message`. New
/// variants are an additive, minor-version change (see the module-level
/// evolution policy); they must not be emitted below the negotiated minor.
#[derive(schemars::JsonSchema, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "type", content = "message")]
pub enum SwitchboardToSupervisor {
    /// Start (or resume) the job identified by the message. Idempotent on
    /// `job_id`: a supervisor already running that job ignores a repeat; it
    /// rejects the command if it does not apply to the current job state. This
    /// makes re-issuing `StartJob` during reconciliation safe.
    StartJob(StartJobMessage),
    /// Stop the job identified by the message. Idempotent: it applies regardless
    /// of state as long as the job exists, and is a no-op once the job is gone.
    StopJob(StopJobMessage),
    /// Ask the supervisor for its current [`ReportedSupervisorStatus`]. The
    /// correlated reply is a [`SupervisorToSwitchboard::StatusResponse`]; see
    /// [`Request`] for the correlation contract. Reconciliation issues exactly
    /// one of these per (re)connect.
    StatusRequest(Request<()>),
    /// Diagnostic notice of a fatal protocol error; see [`ProtocolError`] for
    /// the log → close → reconnect contract.
    ProtocolError(ProtocolError),
}

/// A message sent **from a supervisor to the switchboard**.
///
/// The supervisor is the reporting side: it answers status requests and emits
/// asynchronous events about the job it is executing. It never commands the
/// switchboard, so command variants are deliberately absent from this enum.
///
/// Wire format and evolution rules mirror [`SwitchboardToSupervisor`].
#[derive(schemars::JsonSchema, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "type", content = "message")]
pub enum SupervisorToSwitchboard {
    StatusResponse(Response<ReportedSupervisorStatus>),
    SupervisorEvent(SupervisorEvent),
    /// Diagnostic notice of a fatal protocol error; see [`ProtocolError`] for
    /// the log → close → reconnect contract.
    ProtocolError(ProtocolError),
}

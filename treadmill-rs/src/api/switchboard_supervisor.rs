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
use crate::image::manifest::ImageId;
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
pub const PROTOCOL_MINOR: u16 = 0;

/// A two-level protocol version. `major` is also pinned by the WebSocket
/// subprotocol token; `minor` is negotiated in the handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerHello {
    pub protocol: ProtocolVersion,
    #[serde(default)]
    pub features: BTreeSet<String>,
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
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub struct ProtocolError {
    pub code: ProtocolErrorCode,
    pub detail: String,
}

/// Enumerated protocol error conditions. Each maps to a WebSocket close code in
/// the RFC 6455 private range (4000–4999) via [`ProtocolErrorCode::close_code`].
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "type", content = "message")]
pub enum SwitchboardToSupervisor {
    StartJob(StartJobMessage),
    StopJob(StopJobMessage),
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
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "type", content = "message")]
pub enum SupervisorToSwitchboard {
    StatusResponse(Response<ReportedSupervisorStatus>),
    SupervisorEvent(SupervisorEvent),
    /// Diagnostic notice of a fatal protocol error; see [`ProtocolError`] for
    /// the log → close → reconnect contract.
    ProtocolError(ProtocolError),
}

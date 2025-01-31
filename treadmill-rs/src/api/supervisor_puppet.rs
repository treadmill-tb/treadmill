//! Types used in the interface between supervisors and the puppet
//! daemon running on hosts.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use super::switchboard_supervisor::ParameterValue;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum PuppetReq {
    Ping,
    JobInfo,
    SSHKeys,
    NetworkConfig,
    Parameters,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommandOutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum PuppetEvent {
    /// The puppet has started and completed all initialization necessary for
    /// the host to be used, either to run automated commands or for interactive
    /// sessions.
    Ready,

    /// The puppet is about to shut down.
    ///
    /// If this happens in response to a supervisor-issued event, the puppet
    /// should include its event id here.
    ///
    /// A puppet should inform the supervisor of every host shutdown, but is
    /// allowed to only inform it of ones that have been requested by it.
    Shutdown { supervisor_event_id: Option<u64> },

    /// The puppet is about to reboot.
    ///
    /// If this happens in response to a supervisor-issued event, the puppet
    /// should include its event id here.
    ///
    /// A puppet should inform the supervisor of every host reboot, but is
    /// allowed to only inform it of ones that have been requested by it.
    Reboot { supervisor_event_id: Option<u64> },

    /// Running a command requested by the supervisor failed. This event
    /// includes an error message of what went wrong.
    ///
    /// This event will be the last event related to a given `RunCommand` event
    /// sent by the supervisor.
    RunCommandError {
        supervisor_event_id: u64,
        error: String,
    },

    /// Output of a command (streaming).
    RunCommandOutput {
        supervisor_event_id: u64,
        output: Vec<u8>,
        stream: CommandOutputStream,
    },

    /// Exit code of a command.
    ///
    /// This event will be the last event related to a given `RunCommand` event
    /// sent by the supervisor.
    RunCommandExitCode {
        supervisor_event_id: u64,
        exit_code: Option<i32>,
        killed: bool,
    },

    /// Ask the supervisor to terminate this job.
    ///
    /// This is an infallible operation, and the puppet will not have a chance
    /// to be notified of the successful completion of this operation. Thus we
    /// implement it as an event, not as a request.
    ///
    /// If this termination is performed in response to a request by a
    /// supervisor, the puppet should include this original supervisor event ID
    /// here:
    TerminateJob { supervisor_event_id: Option<u64> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[serde(untagged)]
pub enum PuppetMsg {
    Event {
        puppet_event_id: u64,
        event: PuppetEvent,
    },
    Request {
        request_id: u64,
        request: PuppetReq,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum SupervisorEvent {
    // Events:
    /// The set of SSH keys provided by the supervisor has been updated.
    ///
    /// Based on this event, the puppet may want to update the set of SSH keys
    /// that is authorized to log into the host.
    SSHKeysUpdated,

    /// Request that the host on which the puppet is running be shut
    /// down.
    ///
    /// Before executing the shutdown request, the puppet should acknowledge
    /// that this event will be executed through a `PuppetMsg`. The puppet may
    /// perform a delayed shutdown, to ensure successful message delivery and
    /// perhaps inform any active users. The delay should be kept short, usually
    /// under a minute.
    ShutdownReq,

    /// Request that the host on which the puppet is running be rebooted.
    ///
    /// Before executing the reboot request, the puppet should acknowledge that
    /// this event will be executed through a `PuppetMsg`. The puppet may
    /// perform a delayed reboot, to ensure successful message delivery and
    /// perhaps inform any active users. The delay should be kept short, usually
    /// under a minute.
    RebootReq,

    /// Request to run a command on the host.
    ///
    /// Multiple commands may be executed at the same time (i.e., this should be
    /// non-blocking). The command can be executed from an arbitrary working
    /// directory, with the same permissions as the puppet process (likely, root
    /// user). Related output (stdout, stderr, exit code) should be streamed
    /// back to the supervisor while referencing this event's id.
    ///
    /// The supervisor is responsible for sending the puppet a command line and
    /// environment parameters that the puppet can decode into `OsStr`s valid
    /// for its host platform.
    ///
    /// In case that running a command failed, the puppet should send back a
    /// `RunCommandError`.
    RunCommand {
        cmdline: Vec<u8>,
        environment: Vec<(Vec<u8>, Vec<u8>)>,
    },

    /// Request a command to be killed.
    KillCommand { supervisor_event_id: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub struct Ipv4NetworkConfig {
    pub address: std::net::Ipv4Addr,
    pub prefix_length: u8,
    pub gateway: Option<std::net::Ipv4Addr>,
    pub nameservers: Vec<std::net::Ipv4Addr>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub struct Ipv6NetworkConfig {
    pub address: std::net::Ipv6Addr,
    pub prefix_length: u8,
    pub gateway: Option<std::net::Ipv6Addr>,
    pub nameservers: Vec<std::net::Ipv6Addr>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub struct NetworkConfig {
    pub hostname: String,
    pub interface: Option<String>,
    pub ipv4: Option<Ipv4NetworkConfig>,
    pub ipv6: Option<Ipv6NetworkConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub struct JobInfo {
    pub job_id: Uuid,
    pub host_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum SupervisorResp {
    // Request reponses:
    PingResp,
    JobInfo(JobInfo),
    SSHKeysResp {
        ssh_keys: Vec<String>,
    },
    NetworkConfig(NetworkConfig),
    Parameters {
        parameters: HashMap<String, ParameterValue>,
    },

    // Error responses:
    UnsupportedRequest,
    JobNotFound,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[serde(untagged)]
pub enum SupervisorMsg {
    Event {
        supervisor_event_id: u64,
        event: SupervisorEvent,
    },
    Response {
        request_id: u64,
        response: SupervisorResp,
    },

    // Generic error, when no more specific error applies (for
    // instance, if a message cannot be parsed at all)
    Error {
        message: String,
    },
}

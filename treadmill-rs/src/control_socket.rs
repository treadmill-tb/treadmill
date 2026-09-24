use async_trait::async_trait;
use tracing::{Level, event};
use uuid::Uuid;

use crate::api::supervisor_puppet::{
    self, JobApi, JobInfo, PuppetEvent, PuppetReq, SupervisorResp,
};

/// Supervisor interface for control socket servers.
///
/// A puppet connects to a supervisor through a control socket (e.g., TCP or
/// Unix SeqPacket). These control socket servers will, in turn, deliver
/// requests and events to supervisors. This trait is implemented by supervisors
/// to facilitate the delivery of these events.
#[async_trait]
pub trait Supervisor: Send + Sync + 'static {
    /// Puppet network configuration request.
    ///
    /// Hosts may obtain their network configuration through various means, such
    /// as using DHCP, IPv6 RA, or through static configuration. However, this
    /// request endpoint can be used by puppets that do not have another means
    /// to obtain their network configuration, and where a network connection is
    /// not required to connect to the puppet control socket (such as with
    /// UnixSeqpacket for Linux containers or VirtIO channels in a QEMU VM).
    ///
    /// If this endpoint is supported by the supervisor, it must at least supply
    /// the host's hostname, and may optionally provide IPv4 and IPv6 addresses,
    /// gateways, and a DNS server.
    async fn network_config(
        &self,
        host_id: Uuid,
        job_id: Uuid,
    ) -> Option<supervisor_puppet::NetworkConfig>;

    /// How the job reaches the switchboard API, and its token there.
    ///
    /// Returning `None` leaves the job without switchboard access, and is the
    /// default.
    async fn job_api(&self, _host_id: Uuid, _job_id: Uuid) -> Option<JobApi> {
        None
    }

    /// Generic request handler.
    ///
    /// The default implementation of this method calls out to the other methods
    /// of this trait and should normally not need to be overriden. This method
    /// is to be used by control socket server implementations.
    async fn handle_request(
        &self,
        _request_id: u64,
        req: PuppetReq,
        host_id: Uuid,
        job_id: Uuid,
    ) -> SupervisorResp {
        match req {
            PuppetReq::Ping => SupervisorResp::PingResp,

            PuppetReq::JobInfo => SupervisorResp::JobInfo(JobInfo {
                job_id,
                api: self.job_api(host_id, job_id).await,
            }),

            PuppetReq::NetworkConfig => self
                .network_config(host_id, job_id)
                .await
                .map_or(SupervisorResp::JobNotFound, SupervisorResp::NetworkConfig),
            // Would be required for consumers of this type outside of this
            // crate, as it's marked with `#[non_exhaustive]`. However here we
            // can exhaustively list all request types and force compile-errors
            // if we add some in the future.
            //
            // _ => SupervisorResp::UnsupportedRequest,
        }
    }

    async fn handle_event(
        &self,
        puppet_event_id: u64,
        event: PuppetEvent,
        host_id: Uuid,
        job_id: Uuid,
    ) {
        match event {
            PuppetEvent::Ready => self.puppet_ready(puppet_event_id, host_id, job_id).await,

            PuppetEvent::Shutdown {
                supervisor_event_id,
            } => {
                self.puppet_shutdown(puppet_event_id, supervisor_event_id, host_id, job_id)
                    .await
            }

            PuppetEvent::Reboot {
                supervisor_event_id,
            } => {
                self.puppet_reboot(puppet_event_id, supervisor_event_id, host_id, job_id)
                    .await
            }

            PuppetEvent::RunCommandError { .. }
            | PuppetEvent::RunCommandOutput { .. }
            | PuppetEvent::RunCommandExitCode { .. } => {
                event!(
                    Level::WARN,
                    ?job_id,
                    puppet_event_id,
                    "Received unhandled puppet event: {:?}",
                    event,
                )
            } // Would be required for consumers of this type outside of this
              // crate, as it's marked with `#[non_exhaustive]`. However here we
              // can exhaustively list all event types and force compile-errors
              // if we add some in the future.
              //
              // _ => warn!(Unhandled puppet event!),
        }
    }

    async fn puppet_ready(&self, puppet_event_id: u64, host_id: Uuid, job_id: Uuid);
    async fn puppet_shutdown(
        &self,
        puppet_event_id: u64,
        supervisor_event_id: Option<u64>,
        host_id: Uuid,
        job_id: Uuid,
    );
    async fn puppet_reboot(
        &self,
        puppet_event_id: u64,
        supervisor_event_id: Option<u64>,
        host_id: Uuid,
        job_id: Uuid,
    );
}

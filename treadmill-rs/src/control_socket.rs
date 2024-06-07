use async_trait::async_trait;
use uuid::Uuid;

use crate::api::supervisor_puppet;

/// Supervisor interface for control socket servers.
///
/// A puppet connects to a supervisor through a control socket (e.g., TCP or
/// Unix SeqPacket). These control socket servers will, in turn, deliver
/// requests and events to supervisors. This trait is implemented by supervisors
/// to facilitate the delivery of these events.
#[async_trait]
pub trait Supervisor: Send + Sync + 'static {
    /// Puppet SSH keys request.
    ///
    /// If the supervisor maintains a set of SSH authorized keys that should be
    /// deployed on the puppet, it should return with `Some(<authorized keys
    /// set>)`.
    ///
    /// When a supervisor returns `None`, it indicates that this request is not
    /// supported. A puppet may or may not retry sending SSH keys requests at a
    /// later point in time when the supervisor ever returned `None`. If a
    /// supervisor is able to provide SSH authorized keys generally, but
    /// currently has none configured, it should instead return `Some(vec![])`.
    async fn ssh_keys(&self, job_id: Uuid) -> Option<Vec<String>>;

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
    async fn network_config(&self, job_id: Uuid) -> Option<supervisor_puppet::NetworkConfig>;
}

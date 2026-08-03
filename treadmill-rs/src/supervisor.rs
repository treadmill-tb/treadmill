use std::net::IpAddr;

use serde::Deserialize;
use uuid::Uuid;

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SupervisorCoordConnector {
    RestSSEConnector,
    WsConnector,
    /// Switchboard-less, one-shot local driver (see `treadmill-local-connector`):
    /// the supervisor runs a single job from command-line inputs against its
    /// local OCI store. Intended for local development/testing.
    Local,
}

/// Base configuration object for every supervisor.
///
/// Supervisors should expose this object under the `base` path in their
/// configuration. For instance, for a TOML configuration file:
///
/// ```toml
/// [base]
/// supervisor_id = "e5e7258e-c18b-471d-bc03-8385495b29e4"
/// coord_connector = "ws_connector"
///
/// [ws_connector]
/// some_option = "foo"
///
/// [other_section]
/// hello = "world"
/// ```
#[derive(Deserialize, Debug, Clone)]
pub struct SupervisorBaseConfig {
    pub coord_connector: SupervisorCoordConnector,
    pub supervisor_id: Uuid,
    /// The internal address at which this supervisor's jobs are reachable,
    /// reported to the coordinator when a job starts.
    ///
    /// The address is the supervisor's to state, never the job's to claim: it
    /// is what a gateway dials to reach a job's services, so a job that could
    /// name its own address could point one anywhere. Absent, this supervisor
    /// reports none and its jobs are not reachable through a gateway.
    #[serde(default)]
    pub job_address: Option<IpAddr>,
}

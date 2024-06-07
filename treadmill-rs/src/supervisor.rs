use serde::Deserialize;
use uuid::Uuid;

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SupervisorCoordConnector {
    RestSSEConnector,
    CliConnector,
}

/// Base configuration object for every supervisor.
///
/// Supervisors should expose this object under the `base` path in their
/// configuration. For instance, for a TOML configuration file:
///
/// ```toml
/// [base]
/// supervisor_id = "e5e7258e-c18b-471d-bc03-8385495b29e4"
/// coord_connector = "cli_connector"
///
/// [cli_connector]
/// some_option = "foo"
///
/// [other_section]
/// hello = "world"
/// ```
#[derive(Deserialize, Debug, Clone)]
pub struct SupervisorBaseConfig {
    pub coord_connector: SupervisorCoordConnector,
    pub supervisor_id: Uuid,
}

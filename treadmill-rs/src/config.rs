use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Parser, Debug, Serialize, Deserialize)]
pub struct CliArgs {
    #[clap(short, long)]
    config: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TreadmillConfig {
    pub switchboard: SwitchboardConfig,
    pub supervisor: SupervisorConfig,
    pub puppet: PuppetConfig,
    pub connector: ConnectorConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SwitchboardConfig {
    pub database: Database,
    pub logs: Logs,
    pub public_server: Server,
    pub internal_server: Server,
    pub websocket: WebSocket,
    pub api: Api,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Logs {
    pub dir: PathBuf,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Database {
    pub address: String,
    pub port: u16,
    pub name: String,
    pub auth: DatabaseAuth,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PasswordAuth {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Deserialize, Clone)]
pub enum DatabaseAuth {
    PasswordAuth(PasswordAuth),
}

#[derive(Debug, Deserialize, Clone)]
pub struct Server {
    pub socket_addr: SocketAddr,
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Debug, Deserialize, Clone)]
pub struct WebSocket {
    pub auth: WebSocketAuth,
}

#[derive(Debug, Deserialize, Clone)]
pub struct WebSocketAuth {
    #[serde(with = "humantime_serde")]
    pub per_message_timeout: Duration,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Api {
    #[serde(with = "humantime_serde")]
    pub auth_presession_timeout: Duration,
    #[serde(with = "humantime_serde")]
    pub auth_session_timeout: Duration,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PuppetConfig {
    pub transport: PuppetControlSocketTransport,
    pub tcp_control_socket_addr: Option<SocketAddr>,
    pub authorized_keys_file: Option<PathBuf>,
    pub exit_on_authorized_keys_update_error: bool,
    pub network_config_script: Option<PathBuf>,
    pub exit_on_network_config_error: bool,
    pub parameters_dir: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "lowercase")]
pub enum PuppetControlSocketTransport {
    //#[cfg(feature = "transport_tcp")]
    Tcp,
    AutoDiscover,
}

pub enum ConfigError {
    MissingField(&'static str),
    FigmentError(figment::Error),
}

#[derive(Debug, Deserialize, Clone)]
pub struct SupervisorConfig {
    pub base: SupervisorBaseConfig,
    pub mock: MockConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SupervisorBaseConfig {
    pub supervisor_id: Uuid,
    pub coord_connector: ConnectorType,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "lowercase")]
pub enum ConnectorType {
    Cli,
    Ws,
    RestSSEConnector,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ConnectorConfig {
    pub ws: Option<WsConnectorConfig>,
    pub cli: Option<CliConnectorConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct CliConnectorConfig {
    pub images: std::collections::HashMap<String, String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct WsConnectorConfig {
    pub private_key: String,
    pub switchboard_uri: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MockConfig {
    pub max_parallel_jobs: usize,
    //pub puppet_binary: PathBuf,
}

//##[derive(Parser, Debug, Clone)]
//#pub struct MockSupervisorArgs {
//#    /// Path to the TOML configuration file
//#    #[arg(short, long)]
//#    config_file: PathBuf,
//#
//#    /// Path to the puppet binary to run as the main process of a job
//#    #[arg(short, long)]
//#    puppet_binary: PathBuf,
//#}

pub fn load_config() -> Result<TreadmillConfig, figment::Error> {
    let cli_args = CliArgs::parse();

    let mut figment = Figment::new();

    if let Some(ref path) = cli_args.config {
        figment = figment.merge(Toml::file(path));
    }

    figment
        .merge(Toml::file("treadmill.toml"))
        .merge(Env::prefixed("TREADMILL_"))
        .merge(Serialized::defaults(cli_args))
        .extract()
}

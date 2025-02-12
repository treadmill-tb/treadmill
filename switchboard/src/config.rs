use miette::{IntoDiagnostic, WrapErr};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use treadmill_rs::api::switchboard_supervisor::SocketConfig;
use treadmill_rs::util::chrono::duration as human_duration;

#[derive(Debug, Clone, Deserialize)]
pub struct SwitchboardConfig {
    /// Configuration for connecting to PostgreSQL server.
    pub database: DatabaseConfig,
    /// Configuration of the HTTP server.
    pub server: ServerConfig,
    /// Configuration of the Switchboard service.
    pub service: ServiceConfig,
    /// Configuration of Switchboard logging.
    pub log: LogConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    /// IP address of database server, OR path to Unix socket.
    ///
    /// **NOTE**: if this is a path to a unix socket, `port` MUST be set to `None`.
    pub host: String,
    /// Port of the database server, or `None` if using a Unix socket.
    pub port: Option<u16>,
    /// Name of the database to connect to.
    pub database: String,
    /// Name of the user to connect with.
    pub user: String,
    /// Authentication credentials, if necessary.
    pub auth: Option<DatabaseCredentials>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseCredentials {
    /// Use a password to connect to the database.
    Password(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Socket address to bind to.
    pub bind_address: SocketAddr,
    /// Optional TLS mode for testing only.
    pub testing_only_tls_config: Option<TestingOnlyTlsConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TestingOnlyTlsConfig {
    /// Public key (for TLS).
    pub cert: PathBuf,
    /// Private key (for TLS).
    pub key: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceConfig {
    /// Default lifetime of a user session token.
    #[serde(with = "human_duration")]
    pub default_token_timeout: chrono::TimeDelta,
    /// Default per-job timeout.
    #[serde(with = "human_duration")]
    pub default_job_timeout: chrono::TimeDelta,
    /// Default time a job can be queued before it may be culled.
    #[serde(with = "human_duration")]
    pub default_queue_timeout: chrono::TimeDelta,
    /// Default interval between job-supervisor matching passes
    #[serde(with = "human_duration")]
    pub match_interval: chrono::TimeDelta,
    /// Configuration for the switchboard end of switchboard-supervisor websockets.
    pub socket: SocketConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LogConfig {
    /// Whether to include `console-subscriber`.
    pub use_tokio_console_subscriber: bool,
}

/// Load the switchboard configuration.
pub fn load_configuration(path: Option<&Path>) -> miette::Result<SwitchboardConfig> {
    use figment::providers::{self, Format};
    let f = figment::Figment::new();

    let f = if let Some(p) = path {
        if !p.exists() {
            return Err(miette::miette!(
                "Specified configuration file '{}' does not exist",
                p.display()
            ));
        }

        f.merge(providers::Toml::file(&p))
    } else {
        tracing::info!("No configuration file specified");
        f
    };

    f.merge(providers::Env::prefixed("TML_").split("__"))
        .extract()
        .into_diagnostic()
        .wrap_err("Failed to extract switchboard configuration")
}

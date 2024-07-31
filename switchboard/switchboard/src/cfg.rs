//! Configuration of the switchboard server.
//!
//! See `switchboard/switchboard/config.example.toml` in the git repository for an example.

use miette::{Context, IntoDiagnostic};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Global configuration object.
#[derive(Debug, Deserialize)]
pub struct Config {
    /// How the switchboard should connect to the database.
    pub database: Database,
    /// How the database should handle logging output.
    pub logs: Logs,
    /// Server parameters for the public API server.
    pub server: Server,
    /// Parameters for the websocket backend that supervisors communicate with.
    pub websocket: WebSocket,
    /// General configuration for specific features within the interface.
    pub api: Api,
}

/// Log configuration.
#[derive(Debug, Deserialize)]
pub struct Logs {
    /// Directory in which logs should be placed. (currently unused)
    pub dir: PathBuf,
}

/// Specifies connection information for the database that should be connected to. At the moment,
/// the only database server that is supported is Postgres, and that is unlikely to change in the
/// foreseeable future.
#[derive(Debug, Deserialize)]
pub struct Database {
    /// Host address of the database server.
    pub address: String,
    /// Port at which to connect to the database server.
    pub port: Option<u16>,
    /// Name of the database to connect to (while some databases like MySQL treat the current
    /// database context as changeable during a connected session, Postgres doesn't seem to like
    /// doing this so much).
    pub name: String,
    /// Authentication credential to use on the database.
    pub auth: DatabaseAuth,
}

/// Standard username/password credential-based login to a database.
#[derive(Debug, Deserialize)]
pub struct PasswordAuth {
    pub username: String,
    pub password: String,
}

/// Different methods of authenticating to the database server.
#[derive(Debug, Deserialize)]
pub enum DatabaseAuth {
    /// Use [`PasswordAuth`] to supply a standard username/password credential pair.
    PasswordAuth(PasswordAuth),
}

/// Bind & TLS configuration for a server.
#[derive(Debug, Deserialize)]
pub struct Server {
    /// Socket address to bind to.
    pub socket_addr: SocketAddr,
    /// Optional development-only SSL mode.
    pub dev_mode_ssl: Option<DevModeSsl>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DevModeSsl {
    /// Public key (for TLS). (currently unused)
    pub cert: PathBuf,
    /// Private key (for TLS). (currently unused)
    pub key: PathBuf,
}

/// Websocket configuration
#[derive(Debug, Deserialize)]
pub struct WebSocket {
    /// Parameters which control the authentication process for the websockets endpoint that supervisors
    /// can connect to.
    pub auth: WebSocketAuth,
}

/// WebSocket authentication details.
#[derive(Debug, Deserialize)]
pub struct WebSocketAuth {
    /// How long to keep the connection open while waiting for authentication.
    #[serde(with = "humantime_serde")]
    pub per_message_timeout: Duration,
}

/// Api configuration details.
#[derive(Debug, Deserialize)]
pub struct Api {
    /// How long a fully logged-in user session lasts for.
    #[serde(with = "treadmill_rs::util::chrono::duration")]
    pub auth_session_timeout: chrono::TimeDelta,
    /// Default per-job timeout.
    #[serde(with = "treadmill_rs::util::chrono::duration")]
    pub default_job_timeout: chrono::TimeDelta,
}

pub fn load_config(path: impl AsRef<std::path::Path>) -> miette::Result<Config> {
    let cfg_text = std::fs::read_to_string(path.as_ref())
        .into_diagnostic()
        .wrap_err("Failed to open configuration file")?;
    toml::from_str(&cfg_text)
        .into_diagnostic()
        .wrap_err_with(|| {
            format!(
                "Failed to parse configuration file {}",
                path.as_ref().display()
            )
        })
}

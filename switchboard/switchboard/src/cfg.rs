use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub database: Database,
    pub logs: Logs,
    pub public_server: Server,
    pub internal_server: Server,
    pub websocket: WebSocket,
    pub api: Api,
}

#[derive(Debug, Deserialize)]
pub struct Logs {
    pub dir: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct Database {
    pub address: String,
    pub port: u16,
    pub name: String,
    pub auth: DatabaseAuth,
}

#[derive(Debug, Deserialize)]
pub struct PasswordAuth {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub enum DatabaseAuth {
    PasswordAuth(PasswordAuth),
}

#[derive(Debug, Deserialize)]
pub struct Server {
    pub socket_addr: SocketAddr,
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct WebSocket {
    pub auth: WebSocketAuth,
}

#[derive(Debug, Deserialize)]
pub struct WebSocketAuth {
    #[serde(with = "humantime_serde")]
    pub per_message_timeout: Duration,
}

#[derive(Debug, Deserialize)]
pub struct Api {
    #[serde(with = "humantime_serde")]
    pub auth_presession_timeout: Duration,
    #[serde(with = "humantime_serde")]
    pub auth_session_timeout: Duration,
}

use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub database: Database,
    pub log_dir: PathBuf,
    pub server: Server,
}

#[derive(Debug, Deserialize)]
pub struct Database {
    pub address: String,
    pub port: u16,
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

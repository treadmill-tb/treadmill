use serde::Deserialize;
use std::path::PathBuf;
use thiserror::Error;
use treadmill_rs::api::switchboard::AuthToken;

#[derive(Debug, Clone, Deserialize)]
pub struct WsConnectorConfig {
    pub token: AuthToken,
    pub switchboard_uri: String,
}
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read private key from {0}: {1}")]
    IoError(PathBuf, std::io::Error),
    #[error("invalid authorization token: {0}")]
    InvalidToken(String),
}

pub mod socket_auth;

use async_trait::async_trait;
use base64::Engine;
use ed25519_dalek::pkcs8::DecodePrivateKey;
use ed25519_dalek::SigningKey;
use serde::Deserialize;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{OnceLock, Weak};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::http::{Request, Uri};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use treadmill_rs::api::coord_supervisor::ws_challenge::TREADMILL_WEBSOCKET_PROTOCOL;
use treadmill_rs::connector::{self, JobError, JobState, SupervisorConnector};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize)]
pub struct WsConnectorConfig {
    /// PKCS8 PEM FILE
    private_key: PathBuf,
    switchboard_uri: String,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read private key from {0}: {1}")]
    IoError(PathBuf, std::io::Error),
    #[error("invalid private key: {0}")]
    InvalidKey(ed25519_dalek::pkcs8::Error),
}

#[derive(Debug, Error)]
pub enum WsConnectorError {
    #[error("invalid configuration: {0}")]
    Config(ConfigError),
    #[error("failed to connect to remote host: {0}")]
    Connection(tokio_tungstenite::tungstenite::error::Error),
    #[error("failed to authenticate: {0}")]
    Authentication(socket_auth::AuthError),
}

pub struct WsConnectorInner<S: connector::Supervisor> {
    #[allow(dead_code)]
    supervisor_id: Uuid,
    #[allow(dead_code)]
    config: WsConnectorConfig,
    #[allow(dead_code)]
    supervisor: Weak<S>,
    #[allow(dead_code)]
    connection: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl<S: connector::Supervisor> WsConnectorInner<S> {
    pub async fn connect(
        supervisor_id: Uuid,
        config: WsConnectorConfig,
        supervisor: Weak<S>,
    ) -> Result<Self, WsConnectorError> {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .unwrap();

        let signing_key = SigningKey::from_pkcs8_pem(
            std::fs::read_to_string(&config.private_key)
                .map_err(|e| {
                    WsConnectorError::Config(ConfigError::IoError(config.private_key.clone(), e))
                })?
                .as_str(),
        )
        .map_err(|e| WsConnectorError::Config(ConfigError::InvalidKey(e)))?;

        // As per RFC6455 §4.1:
        // As this is not a browser client and does not match the semantics of one, we do not send
        // an |Origin| header field
        // Currently, we do not use extensions, so "sec-websocket-extensions" is not specified

        let key_buf: [u8; 16] = rand::random();
        let base64_key = base64::prelude::BASE64_STANDARD.encode(&key_buf);
        let uri = Uri::from_str(&config.switchboard_uri).unwrap();
        let req = Request::builder()
            .method("GET")
            // .header("host",... before .uri(... so we don't have to clone
            .header("host", uri.host().unwrap())
            .uri(uri)
            .header("upgrade", "websocket")
            .header("connection", "Upgrade")
            .header("sec-websocket-key", base64_key)
            .header("sec-websocket-protocol", TREADMILL_WEBSOCKET_PROTOCOL)
            .header("sec-websocket-version", "13")
            .body(())
            .unwrap();

        let (mut ws, _resp) =
            tokio_tungstenite::connect_async_tls_with_config(req, None, false, None)
                .await
                .map_err(|e| WsConnectorError::Connection(e))?;

        let _ = socket_auth::authenticate_as_supervisor(&mut ws, supervisor_id, signing_key)
            .await
            .map_err(|e| WsConnectorError::Authentication(e))?;

        Ok(Self {
            supervisor_id,
            config,
            supervisor,
            connection: ws,
        })
    }
}

#[async_trait]
impl<S: connector::Supervisor> SupervisorConnector for WsConnectorInner<S> {
    async fn run(&self) {
        loop {
            return;
        }
    }
    async fn update_job_state(&self, job_id: Uuid, job_state: JobState) {
        tracing::info!(
            "Supervisor provides job state for job {}: {:#?}",
            job_id,
            job_state
        );
    }

    async fn report_job_error(&self, job_id: Uuid, error: JobError) {
        tracing::info!(
            "Supervisor provides job error: job {}, error: {:#?}",
            job_id,
            error,
        );
    }

    async fn send_job_console_log(&self, job_id: Uuid, console_bytes: Vec<u8>) {
        tracing::debug!(
            "Supervisor provides console log: job {}, length: {}, message: {:?}",
            job_id,
            console_bytes.len(),
            String::from_utf8_lossy(&console_bytes)
        );
    }
}

pub struct WsConnector<S: connector::Supervisor> {
    supervisor_id: Uuid,
    config: WsConnectorConfig,
    supervisor: Weak<S>,

    wait_mutex: tokio::sync::Mutex<()>,

    inner_proxy: OnceLock<WsConnectorInner<S>>,
}
impl<S: connector::Supervisor> WsConnector<S> {
    pub fn new(supervisor_id: Uuid, config: WsConnectorConfig, supervisor: Weak<S>) -> Self {
        Self {
            supervisor_id,
            config,
            supervisor,
            wait_mutex: Default::default(),
            inner_proxy: OnceLock::new(),
        }
    }
    async fn assure(&self) -> &WsConnectorInner<S> {
        // So, unfortunately, OnceLock::get_or_init is insufficient, since we need a
        // `WsConnectorInner`, and we can only construct that in an `async` function, so we sort of
        // inline a get-or-init function here.
        if let Some(x) = self.inner_proxy.get() {
            x
        } else {
            {
                let l = self.wait_mutex.lock().await;
                if self.inner_proxy.get().is_none() {
                    let constructed = WsConnectorInner::connect(
                        self.supervisor_id,
                        self.config.clone(),
                        self.supervisor.clone(),
                    )
                    .await
                    .unwrap();

                    // can't unwrap because WsConnectorInner isn't Debug because S: Supervisor
                    // isn't Debug
                    if self.inner_proxy.set(constructed).is_err() {
                        panic!("can't assure: should be impossible");
                    }
                }
                let _ = l;
            }
            self.inner_proxy.get().unwrap()
        }
    }
}

#[async_trait]
impl<S: connector::Supervisor> SupervisorConnector for WsConnector<S> {
    async fn run(&self) {
        self.assure().await.run().await
    }

    async fn update_job_state(&self, job_id: Uuid, job_state: JobState) {
        self.assure()
            .await
            .update_job_state(job_id, job_state)
            .await
    }

    async fn report_job_error(&self, job_id: Uuid, error: JobError) {
        self.assure().await.report_job_error(job_id, error).await
    }

    async fn send_job_console_log(&self, job_id: Uuid, console_bytes: Vec<u8>) {
        self.assure()
            .await
            .send_job_console_log(job_id, console_bytes)
            .await
    }
}

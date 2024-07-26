pub mod socket_auth;

use async_trait::async_trait;
use base64::Engine;
use ed25519_dalek::pkcs8::DecodePrivateKey;
use ed25519_dalek::SigningKey;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Weak};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::{
    self,
    http::{Request, Uri},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use treadmill_rs::api::switchboard_supervisor::{
    self, ws_challenge::TREADMILL_WEBSOCKET_PROTOCOL, Response, SupervisorEvent, SupervisorStatus,
};
use treadmill_rs::connector::{self, JobError, JobState};
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

pub struct WsConnector<S: connector::Supervisor> {
    inner: Arc<Inner<S>>,
}
struct Inner<S: connector::Supervisor> {
    supervisor_id: Uuid,
    config: WsConnectorConfig,
    supervisor: Weak<S>,
    update_rx: Mutex<Option<mpsc::UnboundedReceiver<switchboard_supervisor::Message>>>,
    update_tx: mpsc::UnboundedSender<switchboard_supervisor::Message>,
    last_updated_status: Mutex<SupervisorStatus>,
}

impl<S: connector::Supervisor> WsConnector<S> {
    pub fn new(supervisor_id: Uuid, config: WsConnectorConfig, supervisor: Weak<S>) -> Self {
        let (update_tx, update_rx) = mpsc::unbounded_channel();
        Self {
            inner: Arc::new(Inner {
                supervisor_id,
                config,
                supervisor,
                update_rx: Mutex::new(Some(update_rx)),
                update_tx,
                last_updated_status: Mutex::new(SupervisorStatus::Idle),
            }),
        }
    }
}

impl<S: connector::Supervisor> Inner<S> {
    async fn connect(
        &self,
    ) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, WsConnectorError> {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .unwrap();

        let signing_key = SigningKey::from_pkcs8_pem(
            std::fs::read_to_string(&self.config.private_key)
                .map_err(|e| {
                    WsConnectorError::Config(ConfigError::IoError(
                        self.config.private_key.clone(),
                        e,
                    ))
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
        let uri = Uri::from_str(&self.config.switchboard_uri).unwrap();
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

        let _ = socket_auth::authenticate_as_supervisor(&mut ws, self.supervisor_id, signing_key)
            .await
            .map_err(|e| WsConnectorError::Authentication(e))?;

        Ok(ws)
    }
    async fn handle(&self, message: switchboard_supervisor::Message) {
        match message {
            switchboard_supervisor::Message::StartJob(start_job_request) => {
                let job_id = start_job_request.job_id;
                if let Some(supervisor) = self.supervisor.upgrade() {
                    // TODO: timeout
                    if let Err(error) =
                        connector::Supervisor::start_job(&supervisor, start_job_request).await
                    {
                        self.report_job_error(job_id, error).await;
                        // self.update_tx
                        //     .send(switchboard_supervisor::Message::SupervisorEvent(
                        //         SupervisorEvent::ReportJobError { job_id, error },
                        //     ))
                        //     .unwrap();
                    }
                }
            }
            switchboard_supervisor::Message::StopJob(stop_job_request) => {
                let job_id = stop_job_request.job_id;
                // TODO: timeout
                if let Some(supervisor) = self.supervisor.upgrade() {
                    if let Err(error) =
                        connector::Supervisor::stop_job(&supervisor, stop_job_request).await
                    {
                        self.report_job_error(job_id, error).await;
                        // self.update_tx
                        //     .send(switchboard_supervisor::Message::SupervisorEvent(
                        //         SupervisorEvent::ReportJobError { job_id, error },
                        //     ))
                        //     .unwrap();
                    }
                }
            }
            switchboard_supervisor::Message::StatusRequest(switchboard_supervisor::Request {
                request_id,
                message: (),
            }) => {
                let status = self.last_updated_status.lock().await.clone();
                self.update_tx
                    .send(switchboard_supervisor::Message::StatusResponse(Response {
                        response_to_request_id: request_id,
                        message: status,
                    }))
                    .unwrap();
            }
            switchboard_supervisor::Message::SupervisorEvent(_) => {
                // shouldn't happen
                unimplemented!()
            }
            switchboard_supervisor::Message::StatusResponse(_) => {
                // Shouldn't happen
                unimplemented!()
            }
        }
    }
}

#[async_trait]
impl<S: connector::Supervisor> connector::SupervisorConnector for WsConnector<S> {
    async fn run(&self) {
        Inner::run(&self.inner).await
    }

    async fn update_job_state(&self, job_id: Uuid, job_state: JobState) {
        self.inner.update_job_state(job_id, job_state).await
    }

    async fn report_job_error(&self, job_id: Uuid, error: JobError) {
        self.inner.report_job_error(job_id, error).await
    }

    async fn send_job_console_log(&self, job_id: Uuid, console_bytes: Vec<u8>) {
        self.inner.send_job_console_log(job_id, console_bytes).await
    }
}

impl<S: connector::Supervisor> Inner<S> {
    async fn run(self: &Arc<Self>) {
        let mut socket = self.connect().await.unwrap();
        let mut update_rx = self.update_rx.lock().await.take().unwrap();
        loop {
            tokio::select! {
                msg = update_rx.recv() => {
                    let msg = msg.unwrap();
                    // let to_msg = switchboard_supervisor::Message::SupervisorEvent(msg);
                    let stringified = serde_json::to_string(&msg).unwrap();

                    if let Err(e) = socket.send(tungstenite::Message::Text(stringified)).await {
                        tracing::error!("Failed to send message: {e}");
                    }
                }
                msg = socket.next() => {
                    let msg = msg.unwrap();
                    match msg {
                        Ok(msg) => {
                            match msg {
                                tungstenite::Message::Text(s) => {
                                    let msg : switchboard_supervisor::Message = match serde_json::from_str(&s) {
                                        Ok(m) => m,
                                        Err(e) => {
                                            tracing::error!("Failed to deserialize message: {e}");
                                            continue
                                        }
                                    };
                                    // This is the reason we have separate WsConnector and Inner
                                    // types: Supervisor wants to have a <dyn SupervisorConnector>
                                    // (because the SupervisorConnectors right now take
                                    // <S: Supervisor>, and we need to avoid recursive types), so
                                    // we need something that is object-safe; however, if we want to
                                    // be able to serve a status request while a job is being
                                    // started, we need to tokio::spawn. For lifetime reasons, then,
                                    // we need self to be 'static.
                                    // However, to be object-safe, it won't work for
                                    // SupervisorConnector::run to take self:&Arc<Self>; therefore
                                    // we have an interior type that lives inside an Arc.
                                    let this = Arc::clone(self);
                                    let _jh = tokio::spawn(async move { this.handle(msg).await });
                                }
                                tungstenite::Message::Binary(_) => {
                                    unimplemented!()
                                }
                                tungstenite::Message::Ping(_) => {
                                    tracing::info!("PING");
                                }
                                tungstenite::Message::Pong(_) => {
                                    tracing::info!("PONG");
                                }
                                tungstenite::Message::Close(cf) => {
                                    if let Some(cf) = cf {
                                        tracing::warn!("Received close message; code = {}, reason = {}", cf.code, cf.reason);
                                    } else {
                                        tracing::warn!("Received close message with no close frame");
                                    }
                                    return
                                }
                                tungstenite::Message::Frame(_) => {unreachable!()}
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to receive message on websocket: {e}");
                        }
                    }
                }
            }
        }
    }
    async fn update_job_state(&self, job_id: Uuid, job_state: JobState) {
        tracing::info!(
            "Supervisor provides job state for job {}: {:#?}",
            job_id,
            job_state
        );
        {
            let mut lus_lg = self.last_updated_status.lock().await;
            // MC: We do not currently handle this case, since there is no extant usage, and I am
            //     unsure of the semantics of JobState::Failed.
            assert!(!matches!(job_state, JobState::Failed { .. }));
            // Finished is an event, not a state.
            if matches!(job_state, JobState::Finished { .. }) {
                *lus_lg = SupervisorStatus::Idle;
            } else {
                *lus_lg = SupervisorStatus::OngoingJob {
                    job_id,
                    job_state: job_state.clone(),
                };
            }
        }
        if let Err(e) = self
            .update_tx
            .send(switchboard_supervisor::Message::SupervisorEvent(
                SupervisorEvent::UpdateJobState { job_id, job_state },
            ))
        {
            tracing::error!("failed to send job state update to runloop: {e}")
        }
    }

    async fn report_job_error(&self, job_id: Uuid, error: JobError) {
        tracing::info!(
            "Supervisor provides job error: job {}, error: {:#?}",
            job_id,
            error,
        );
        {
            // An error occurred-according to my understanding, the supervisor should cancel the job
            // and return to an idle state. 'Error' is not a state, but Idle is.
            let mut lus_lg = self.last_updated_status.lock().await;
            *lus_lg = SupervisorStatus::Idle;
        }
        if let Err(e) = self
            .update_tx
            .send(switchboard_supervisor::Message::SupervisorEvent(
                SupervisorEvent::ReportJobError { job_id, error },
            ))
        {
            tracing::error!("failed to report job error to runloop: {e}")
        }
    }

    async fn send_job_console_log(&self, job_id: Uuid, console_bytes: Vec<u8>) {
        tracing::debug!(
            "Supervisor provides console log: job {}, length: {}, message: {:?}",
            job_id,
            console_bytes.len(),
            String::from_utf8_lossy(&console_bytes)
        );
        if let Err(e) = self
            .update_tx
            .send(switchboard_supervisor::Message::SupervisorEvent(
                SupervisorEvent::SendJobConsoleLog {
                    job_id,
                    console_bytes,
                },
            ))
        {
            tracing::error!("failed to send job console log to runloop: {e}")
        }
    }
}

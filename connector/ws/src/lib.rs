use async_trait::async_trait;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::{
    self,
    http::{Request, StatusCode, Uri},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::instrument;
use treadmill_rs::api::switchboard::AuthToken;
use treadmill_rs::api::switchboard_supervisor::websocket::TREADMILL_WEBSOCKET_CONFIG;
use treadmill_rs::api::switchboard_supervisor::{
    self, websocket::TREADMILL_WEBSOCKET_PROTOCOL, JobUserExitStatus, ReportedSupervisorStatus,
    Response, SocketConfig, SupervisorEvent, SupervisorJobEvent,
};
use treadmill_rs::connector::{self, JobError, RunningJobState};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum WsConnectorConfigToken {
    TokenFile { token_file: PathBuf },
    Token { token: AuthToken },
}

#[derive(Debug, Clone, Deserialize)]
pub struct WsConnectorConfig {
    #[serde(flatten)]
    token: WsConnectorConfigToken,
    switchboard_uri: String,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read token from {0}: {1}")]
    IoError(PathBuf, std::io::Error),
    #[error("invalid authorization token: {0}")]
    InvalidToken(String),
}

#[derive(Debug, Error)]
pub enum WsConnectorError {
    #[error("invalid configuration: {0}")]
    Config(ConfigError),
    #[error("failed to connect to remote host: {0}")]
    Connection(tokio_tungstenite::tungstenite::error::Error),
    #[error("failed to authenticate")]
    Authentication,
    #[error("failed to install CryptoProvider for WebSocket TLS")]
    TLSCryptoProvider,
    #[error("Couldn't parse URL built from configured values: {0}")]
    InvalidURL(String),
    #[error("Failed to receive Treadmill socket configuration")]
    SocketConfig,
}

// We need to spawn tokio tasks if we want to be able to parallelize jobs; however, this requires
// 'static `Fn`s, and due to the way that the SupervisorConnector is written (and the way it's used)
// it can only use `&self`. Therefore, it is most convenient to use an `Arc` over an inner type
// since this allows us to get `self: &Arc<Self>` which has 'static.
#[derive(Debug)]
pub struct WsConnector<S: connector::Supervisor> {
    inner: Arc<Inner<S>>,
    shutdown_tx: watch::Sender<bool>,
}
#[derive(Debug)]
struct Inner<S: connector::Supervisor> {
    supervisor_id: Uuid,
    config: WsConnectorConfig,
    /// A reference to the supervisor is needed to actualise incoming messages into actual
    /// invocations on the supervisor. Since the supervisor is expected to have an
    /// `Arc<dyn SupervisorConnector>`, this ends up being a [`Weak`] ref.
    supervisor: Weak<S>,
    /// To receive from an [`tokio::mpsc::UnboundedReceiver`], an `&mut` reference is necessary.
    /// This cannot be accomplished through the [`Arc`] around [`Inner`], so we use a [`Mutex`] for
    /// interior mutability.
    update_rx: Mutex<mpsc::UnboundedReceiver<switchboard_supervisor::Message>>,
    /// This acts as an interior conduit from the `update_*` methods to the `run()` method.
    update_tx: mpsc::UnboundedSender<switchboard_supervisor::Message>,
    /// The most recent status that was received from the supervisor.
    last_updated_status: Mutex<ReportedSupervisorStatus>,

    shutdown_rx: watch::Receiver<bool>,
}

impl<S: connector::Supervisor> WsConnector<S> {
    pub fn new(supervisor_id: Uuid, config: WsConnectorConfig, supervisor: Weak<S>) -> Self {
        let (update_tx, update_rx) = mpsc::unbounded_channel();

        // Create a watch channel with an initial value of `false` (i.e., not shutting down yet)
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        Self {
            inner: Arc::new(Inner {
                supervisor_id,
                config,
                supervisor,
                update_rx: Mutex::new(update_rx),
                update_tx,
                last_updated_status: Mutex::new(ReportedSupervisorStatus::Idle),
                shutdown_rx,
            }),
            shutdown_tx,
        }
    }

    /// Signal that we want to shut down gracefully. We can ignore errors from `send`,
    /// because it only errors if all receivers have dropped, which can’t really happen
    /// here unless the process is already shutting down.
    pub fn request_shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    pub fn shutdown_requested(&self) -> bool {
        *self.inner.shutdown_rx.borrow()
    }
}

// As mentioned above, the `connector::SupervisorConnector` implementation is not capable of
// implementing the functionality, so we forward to `Inner`, which is.
#[async_trait]
impl<S: connector::Supervisor> connector::SupervisorConnector for WsConnector<S> {
    async fn run(&self) {
        Inner::run(&self.inner).await
    }

    async fn update_event(&self, supervisor_event: SupervisorEvent) {
        match supervisor_event {
            SupervisorEvent::JobEvent { job_id, event } => match event {
                SupervisorJobEvent::StateTransition {
                    new_state,
                    status_message: _, /* TODO: handle */
                } => self.inner.update_job_state(job_id, new_state).await,
                SupervisorJobEvent::DeclareExitStatus {
                    user_exit_status,
                    host_output,
                } => {
                    self.inner
                        .declare_exit_status(job_id, user_exit_status, host_output)
                        .await
                }
                SupervisorJobEvent::Error { error } => {
                    self.inner.report_job_error(job_id, error).await
                }
                SupervisorJobEvent::ConsoleLog { console_bytes } => {
                    self.inner.send_job_console_log(job_id, console_bytes).await
                }
            },
        }
    }
}

static INSTALL_CRYPTO_PROVIDER_ONCE: AtomicBool = AtomicBool::new(false);
fn assure_crypto_provider() -> Result<(), WsConnectorError> {
    if !INSTALL_CRYPTO_PROVIDER_ONCE.swap(true, Ordering::SeqCst) {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .map_err(|_| WsConnectorError::TLSCryptoProvider)?;
    }
    Ok(())
}

impl<S: connector::Supervisor> Inner<S> {
    /// Try to connect with the switchboard using the configuration specified to
    /// [`WsConnector::new`].
    // Unfortunately, the constructor cannot be async, since the constructor is called inside
    // [`Arc::new_cyclic`], so we have a separate connect() function that is called at the beginning
    // of run().
    #[instrument(skip(self))]
    async fn connect(
        &self,
    ) -> Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, SocketConfig), WsConnectorError> {
        assure_crypto_provider()?;

        let token = match &self.config.token {
            WsConnectorConfigToken::TokenFile { token_file } => {
                let token_base64 = tokio::fs::read(&token_file).await.map_err(|io_err| {
                    WsConnectorError::Config(ConfigError::IoError(token_file.clone(), io_err))
                })?;

                base64::prelude::BASE64_STANDARD
                    .decode(token_base64.trim_ascii()) // Remove leading & trailing whitespace
                    .map_err(|base64_decode_err| base64_decode_err.to_string())
                    .and_then(|token_bytes| {
                        Ok(AuthToken(token_bytes.as_slice().try_into().map_err(
                            |length_mismatch_err: std::array::TryFromSliceError| {
                                length_mismatch_err.to_string()
                            },
                        )?))
                    })
                    .map_err(|formatted_err| {
                        WsConnectorError::Config(ConfigError::InvalidToken(formatted_err))
                    })?
            }

            WsConnectorConfigToken::Token { token } => token.clone(),
        };

        // .expect() is okay here: the token was originally base64-encoded so there really doesn't
        // seem to be a way that this to_string() could fail other than an abject failure of the
        // entire system.
        // let token_ser_string =
        //     serde_json::to_string(&self.config.token).expect("failed to re-serialize token");
        let token_ser_string = token.encode_for_http();

        // sec-websocket-key is 16 random bytes, encoded with the standard base64.
        let key_buf: [u8; 16] = rand::random();
        let base64_key = base64::prelude::BASE64_STANDARD.encode(&key_buf);
        let uri = Uri::from_str(&format!(
            "{}/api/v1/supervisors/{}/connect",
            self.config.switchboard_uri, self.supervisor_id,
        ))
        .map_err(|invalid_url| WsConnectorError::InvalidURL(invalid_url.to_string()))?;
        // As per RFC6455 §4.1:
        // As this is not a browser client and does not match the semantics of one, we do not send
        // an `origin` header field.
        // Currently, we do not use extensions, so "sec-websocket-extensions" is not specified
        // To the best of my knowledge, the order of HTTP headers is of no particular importance in
        // this case.
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
            .header("authorization", format!("Bearer {token_ser_string}"))
            .body(())
            // It should not be possible to cause this to error by runtime misconfiguration. It
            // should only be possible by means of mucking something up in the code.
            // Therefore, .expect() is OK in this context.
            .expect("Failed to build HTTP Request (should be impossible)");

        tracing::debug!("Request = {req:?}");

        // While there _is_ a separate `connect_async_tls_with_config`, it's sufficient to connect
        let (ws, resp) = tokio_tungstenite::connect_async(req).await.map_err(|e| {
            tracing::error!("Failed to connect: {e}");
            WsConnectorError::Connection(e)
        })?;

        // Even if the connection went through, it's still possible that the request was denied
        // (e.g. if the supervisor ID or token is wrong, in which case the response will have status
        // 403 FORBIDDEN).
        tracing::debug!("Received response from switchboard: {resp:?}");
        match resp.status() {
            StatusCode::SWITCHING_PROTOCOLS => {
                // This is the expected response of a WebSocket connection.
                tracing::info!("Authenticated successfully, switching protocols!");
            }
            StatusCode::FORBIDDEN => {
                tracing::error!(
                    "Received 403 FORBIDDEN from switchboard; supervisor ID-token pair is invalid, please check configuration."
                );
                return Err(WsConnectorError::Authentication);
            }
            status => {
                tracing::error!(
                    "Received unexpected response from switchboard with status: {status:?}"
                );
                // TODO: Not really sure what else to do with this, but I don't think
                // `Authentication` is the right error to return.
                return Err(WsConnectorError::Authentication);
            }
        }
        let socket_config_val =
            resp.headers()
                .get(TREADMILL_WEBSOCKET_CONFIG)
                .ok_or_else(|| {
                    tracing::error!("Response did not include tml-socket-config header");
                    WsConnectorError::SocketConfig
                })?;
        let socket_config_str = socket_config_val.to_str().map_err(|e| {
            tracing::error!("Failed to parse tml-socket-config header value: {e}");
            WsConnectorError::SocketConfig
        })?;
        let socket_config: SocketConfig = serde_json::from_str(socket_config_str).map_err(|e| {
            tracing::error!("Failed to deserialize tml-socket-config header value: {e}");
            WsConnectorError::SocketConfig
        })?;

        Ok((ws, socket_config))
    }

    /// Handle a message received from the switchboard.
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

impl<S: connector::Supervisor> Inner<S> {
    // This function returns when the connection closes. Reconnection must be handled externally.
    async fn run(self: &Arc<Self>) {
        let (mut socket, socket_config) = match self.connect().await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to connect: {e}");
                return;
            }
        };
        let mut update_rx = self.update_rx.lock().await;

        let mut shutdown_rx = self.shutdown_rx.clone();

        // No special on-connection behaviour is necessary: the switchboard will request the
        // supervisor status, and use that to determine if the information it has on file for this
        // supervisor's current job state is correct, which falls under the normal request handling
        // flow.

        tracing::info!("Received switchboard socket configuration: {socket_config:?}");

        let keepalive = socket_config.keepalive.keepalive.to_std().inspect_err(|e| {
            tracing::error!("Switchboard-specified keepalive couldn't be converted to Duration: {e}; falling back to default 10s keepalive");
        }).unwrap_or(tokio::time::Duration::from_secs(10));
        let mut interval = tokio::time::interval(keepalive);
        let mut last_received_ping = tokio::time::Instant::now();

        loop {
            // Check if the watch channel has changed
            if *self.shutdown_rx.borrow() && self.is_idle().await {
                tracing::info!("Shutdown requested, and supervisor is idle; exiting run().");
                return;
            }

            tokio::select! {
                // Check if the watch channel has changed
                changed_result = shutdown_rx.changed() => {
                    if changed_result.is_ok() {
                        // We got a new value in the watch channel. Check if it's `true`:
                        let shutdown_requested = *self.shutdown_rx.borrow();
                        if shutdown_requested && self.is_idle().await {
                            tracing::info!("Shutdown requested (watch changed) and idle => exit run().");
                            return;
                        }
                    } else {
                        // If `changed()` errors, it means all watch senders have dropped. Probably safe to ignore.
                        tracing::warn!("shutdown_rx.changed() returned an error (sender dropped?)");
                    }
                }
                _ = interval.tick() =>  {
                    tracing::trace!(target: "tml_ws_connector:ping", "keepalive tick");
                    let elapsed = last_received_ping.elapsed();
                    if elapsed > keepalive {
                        tracing::error!("Haven't received a PING in {elapsed:?}, exiting socket control loop");
                        return
                    }
                }
                msg = update_rx.recv() => {
                    let msg = msg.unwrap();
                    let stringified = serde_json::to_string(&msg).unwrap();

                    tracing::debug!("Sending message: {msg:?}");

                    if let Err(e) = socket.send(tungstenite::Message::Text(stringified)).await {
                        tracing::error!("Failed to send message: {e}");
                    }
                }
                msg = socket.next() => {
                    let websocket_message = match msg {
                        Some(Ok(msg)) => {
                            msg
                        }
                        Some(Err(e)) => {
                            tracing::error!("Failed to receive message on websocket: {e}");
                            continue
                        }
                        None => {
                            tracing::warn!("WebSocket stream closed unexpectedly");
                            // This is typically because the server closed the connection via a kill
                            // signal or similar event.
                            return;
                        }
                    };

                    match websocket_message {
                        tungstenite::Message::Text(s) => {
                            tracing::debug!("Received text message from websocket: {s}");
                            let msg = match serde_json::from_str::<switchboard_supervisor::Message>(&s) {
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
                            tracing::error!("Received binary message from switchboard");
                        }
                        tungstenite::Message::Ping(_) => {
                            tracing::trace!(target: "tml_ws_connector:ping", "Received PING from switchboard");
                            last_received_ping = tokio::time::Instant::now();
                        }
                        tungstenite::Message::Pong(_) => {
                            tracing::error!("Received PONG from switchboard");
                        }
                        tungstenite::Message::Close(cf) => {
                            if let Some(cf) = cf {
                                tracing::warn!("Received close message; code = {}, reason = {}", cf.code, cf.reason);
                            } else {
                                tracing::warn!("Received close message with no close frame");
                            }
                            return
                        }
                        tungstenite::Message::Frame(_) => {
                            tracing::error!("Received `Frame` message from switchboard; this is almost certainly an error");
                            // See the docs for Message::Frame
                            unreachable!()
                        }
                    }
                }
            }
        }

        // unreachable
    }

    async fn is_idle(&self) -> bool {
        let st = self.last_updated_status.lock().await;
        matches!(*st, ReportedSupervisorStatus::Idle)
    }

    async fn update_job_state(&self, job_id: Uuid, job_state: RunningJobState) {
        tracing::info!(
            "Supervisor provides job state for job {}: {:#?}",
            job_id,
            job_state
        );
        // First, update the supervisor status based on the job state.
        {
            let mut lus_lg = self.last_updated_status.lock().await;
            // Finished is an event, not a state.
            if matches!(job_state, RunningJobState::Terminated) {
                *lus_lg = ReportedSupervisorStatus::Idle;
            } else {
                *lus_lg = ReportedSupervisorStatus::OngoingJob {
                    job_id,
                    job_state: job_state.clone(),
                };
            }
        }
        // Send the update to the run() loop, which will forward it to the switchboard
        if let Err(e) = self
            .update_tx
            .send(switchboard_supervisor::Message::SupervisorEvent(
                SupervisorEvent::JobEvent {
                    job_id,
                    event: SupervisorJobEvent::StateTransition {
                        new_state: job_state,
                        status_message: None,
                    },
                },
            ))
        {
            tracing::error!("failed to send job state update to runloop: {e}")
        }
    }

    async fn declare_exit_status(
        &self,
        job_id: Uuid,
        user_exit_status: JobUserExitStatus,
        host_output: Option<String>,
    ) {
        tracing::info!(
            "Supervisor provides exit status: job {}, status {:#?}",
            job_id,
            user_exit_status
        );
        if let Err(e) = self
            .update_tx
            .send(switchboard_supervisor::Message::SupervisorEvent(
                SupervisorEvent::JobEvent {
                    job_id,
                    event: SupervisorJobEvent::DeclareExitStatus {
                        user_exit_status,
                        host_output,
                    },
                },
            ))
        {
            tracing::error!("failed to send job exit status update to runloop: {e}");
        }
    }

    async fn report_job_error(&self, job_id: Uuid, error: JobError) {
        tracing::info!(
            "Supervisor provides job error: job {}, error: {:#?}",
            job_id,
            error,
        );
        // Set the supervisor status
        {
            // An error occurred. Errors (in the supervisor) are always fatal to the job.
            // 'Error' is not a state, but 'Idle' is.
            let mut lus_lg = self.last_updated_status.lock().await;
            *lus_lg = ReportedSupervisorStatus::Idle;
        }
        // Send the error to the run() loop, which will forward it to the switchboard
        if let Err(e) = self
            .update_tx
            .send(switchboard_supervisor::Message::SupervisorEvent(
                SupervisorEvent::JobEvent {
                    job_id,
                    event: SupervisorJobEvent::Error { error },
                },
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
        // This is a bit of an anachronism. Truthfully, we shouldn't be doing this at all, but at
        // least for now, we retain support.
        if let Err(e) = self
            .update_tx
            .send(switchboard_supervisor::Message::SupervisorEvent(
                SupervisorEvent::JobEvent {
                    job_id,
                    event: SupervisorJobEvent::ConsoleLog { console_bytes },
                },
            ))
        {
            tracing::error!("failed to send job console log to runloop: {e}")
        }
    }
}

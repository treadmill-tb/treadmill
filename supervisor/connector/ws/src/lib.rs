use async_trait::async_trait;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::net::IpAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::{
    self,
    http::{Request, StatusCode, Uri},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{instrument, warn};
use treadmill_rs::api::switchboard::AuthToken;
use treadmill_rs::api::switchboard_supervisor::websocket::{
    TREADMILL_PROTOCOL_MINOR_HEADER, TREADMILL_WEBSOCKET_CONFIG,
};
use treadmill_rs::api::switchboard_supervisor::{
    self, JobService, PROTOCOL_MINOR, ProtocolVersion, ReportedSupervisorStatus, Response,
    ServerHello, SupervisorEvent, SupervisorJobEvent, SupervisorToSwitchboard,
    SwitchboardToSupervisor, TaskExitStatus, websocket::TREADMILL_WEBSOCKET_PROTOCOL,
};
use treadmill_rs::connector::{self, CoordCommand, JobError, RunningJobState};
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
    /// How often this connector sends a WebSocket PING to the switchboard.
    /// Local to the supervisor side; deliberately not part of the protocol
    /// handshake (each side runs its own keepalive). Defaults to 10s.
    #[serde(with = "humantime_serde", default = "default_ping_interval")]
    ping_interval: Duration,
    /// How long this connector waits for a PONG before declaring the switchboard
    /// dead and dropping the connection. Defaults to 60s.
    #[serde(with = "humantime_serde", default = "default_pong_timeout")]
    pong_timeout: Duration,
}

fn default_ping_interval() -> Duration {
    Duration::from_secs(10)
}
fn default_pong_timeout() -> Duration {
    Duration::from_secs(60)
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
pub struct WsConnector {
    inner: Arc<Inner>,
    shutdown_tx: watch::Sender<bool>,
}
#[derive(Debug)]
struct Inner {
    supervisor_id: Uuid,
    config: WsConnectorConfig,
    /// Incoming messages are translated into [`CoordCommand`]s and pushed into
    /// the supervisor's command channel. The connector holds no reference to
    /// the supervisor itself.
    commands: mpsc::Sender<CoordCommand>,
    /// To receive from an [`tokio::mpsc::UnboundedReceiver`], an `&mut` reference is necessary.
    /// This cannot be accomplished through the [`Arc`] around [`Inner`], so we use a [`Mutex`] for
    /// interior mutability.
    update_rx: Mutex<mpsc::UnboundedReceiver<SupervisorToSwitchboard>>,
    /// This acts as an interior conduit from the `update_*` methods to the `run()` method.
    update_tx: mpsc::UnboundedSender<SupervisorToSwitchboard>,

    shutdown_rx: watch::Receiver<bool>,
}

impl WsConnector {
    pub fn new(
        supervisor_id: Uuid,
        config: WsConnectorConfig,
        commands: mpsc::Sender<CoordCommand>,
    ) -> Self {
        let (update_tx, update_rx) = mpsc::unbounded_channel();

        // Create a watch channel with an initial value of `false` (i.e., not shutting down yet)
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        Self {
            inner: Arc::new(Inner {
                supervisor_id,
                config,
                commands,
                update_rx: Mutex::new(update_rx),
                update_tx,
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
}

// As mentioned above, the `connector::SupervisorConnector` implementation is not capable of
// implementing the functionality, so we forward to `Inner`, which is.
#[async_trait]
impl connector::SupervisorConnector for WsConnector {
    async fn run(&self) -> Result<(), ()> {
        Inner::run(&self.inner).await
    }

    async fn emit(&self, supervisor_event: SupervisorEvent) {
        match supervisor_event {
            SupervisorEvent::JobEvent { job_id, event } => match event {
                SupervisorJobEvent::StateTransition {
                    new_state,
                    status_message: _, /* TODO: handle */
                } => self.inner.update_job_state(job_id, new_state).await,
                SupervisorJobEvent::DeclareExitStatus { outcome, message } => {
                    self.inner
                        .declare_exit_status(job_id, outcome, message)
                        .await
                }
                SupervisorJobEvent::Error { error } => {
                    self.inner.report_job_error(job_id, error).await
                }
                SupervisorJobEvent::JobNetworkAddress { address } => {
                    self.inner.report_job_network_address(job_id, address).await
                }
                SupervisorJobEvent::JobServiceSet { services } => {
                    self.inner.report_job_service_set(job_id, services).await
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

impl Inner {
    /// Try to connect with the switchboard using the configuration specified to
    /// [`WsConnector::new`].
    // Unfortunately, the constructor cannot be async, so we have a separate
    // connect() function that is called at the beginning of run().
    #[instrument(skip(self))]
    async fn connect(
        &self,
    ) -> Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, ServerHello), WsConnectorError> {
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

            WsConnectorConfigToken::Token { token } => *token,
        };

        // .expect() is okay here: the token was originally base64-encoded so there really doesn't
        // seem to be a way that this to_string() could fail other than an abject failure of the
        // entire system.
        // let token_ser_string =
        //     serde_json::to_string(&self.config.token).expect("failed to re-serialize token");
        let token_ser_string = token.encode_for_http();

        // sec-websocket-key is 16 random bytes, encoded with the standard base64.
        let key_buf: [u8; 16] = rand::random();
        let base64_key = base64::prelude::BASE64_STANDARD.encode(key_buf);
        let uri = Uri::from_str(&format!(
            "{}/api/v1/hosts/{}/connect",
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
            // Advertise our protocol minor for handshake negotiation; the major
            // rides the subprotocol token above.
            .header(TREADMILL_PROTOCOL_MINOR_HEADER, PROTOCOL_MINOR)
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
        let server_hello_val = resp
            .headers()
            .get(TREADMILL_WEBSOCKET_CONFIG)
            .ok_or_else(|| {
                tracing::error!("Response did not include tml-socket-config header");
                WsConnectorError::SocketConfig
            })?;
        let server_hello_str = server_hello_val.to_str().map_err(|e| {
            tracing::error!("Failed to parse tml-socket-config header value: {e}");
            WsConnectorError::SocketConfig
        })?;
        let server_hello: ServerHello = serde_json::from_str(server_hello_str).map_err(|e| {
            tracing::error!("Failed to deserialize tml-socket-config header value: {e}");
            WsConnectorError::SocketConfig
        })?;

        // Major must match (the subprotocol token should already have enforced
        // this, but verify defensively); the effective minor is the lower of the
        // two advertised minors.
        if server_hello.protocol.major != ProtocolVersion::CURRENT.major {
            tracing::error!(
                client_major = ProtocolVersion::CURRENT.major,
                server_major = server_hello.protocol.major,
                "Switchboard speaks an incompatible protocol major; closing."
            );
            return Err(WsConnectorError::SocketConfig);
        }
        // `min` is a no-op while PROTOCOL_MINOR is 0, but it is the correct
        // negotiation once minors diverge; keep it rather than hard-code 0.
        #[allow(clippy::unnecessary_min_or_max)]
        let effective_minor = PROTOCOL_MINOR.min(server_hello.protocol.minor);
        tracing::info!(
            client_minor = PROTOCOL_MINOR,
            server_minor = server_hello.protocol.minor,
            effective_minor,
            "Negotiated protocol minor with switchboard."
        );

        Ok((ws, server_hello))
    }

    /// Handle a message received from the switchboard.
    ///
    /// Commands enter the supervisor's channel in the order they arrived on the
    /// socket; only the wait for an acknowledgement is detached, so a slow
    /// teardown does not hold up the socket's keepalive.
    async fn handle(self: &Arc<Self>, message: SwitchboardToSupervisor) {
        match message {
            SwitchboardToSupervisor::StartJob(start_job_request) => {
                self.command(CoordCommand::StartJob(start_job_request))
                    .await;
            }
            SwitchboardToSupervisor::TerminateJob(terminate_job_request) => {
                let job_id = terminate_job_request.job_id;
                let (ack, acked) = oneshot::channel();
                self.command(CoordCommand::TerminateJob { job_id, ack })
                    .await;

                let this = Arc::clone(self);
                tokio::spawn(async move {
                    if let Ok(Err(error)) = acked.await {
                        this.report_job_error(job_id, error).await;
                    }
                });
            }
            SwitchboardToSupervisor::RemoveJob(remove_job_request) => {
                let job_id = remove_job_request.job_id;
                let (ack, acked) = oneshot::channel();
                self.command(CoordCommand::RemoveJob { job_id, ack }).await;

                let this = Arc::clone(self);
                tokio::spawn(async move {
                    if let Ok(Err(error)) = acked.await {
                        this.report_job_error(job_id, error).await;
                    }
                });
            }
            SwitchboardToSupervisor::StatusRequest(switchboard_supervisor::Request {
                request_id,
                message: (),
            }) => {
                let (reply, replied) = oneshot::channel();
                self.command(CoordCommand::StatusRequest { reply }).await;

                let this = Arc::clone(self);
                tokio::spawn(async move {
                    let Ok(status) = replied.await else {
                        return;
                    };
                    if let Err(e) =
                        this.update_tx
                            .send(SupervisorToSwitchboard::StatusResponse(Response {
                                response_to_request_id: request_id,
                                message: status,
                            }))
                    {
                        tracing::error!("failed to send status response to runloop: {e}");
                    }
                });
            }
            SwitchboardToSupervisor::ProtocolError(err) => {
                // Diagnostic only: the switchboard SHOULD follow this with a
                // Close frame. We log and let the close path tear down the
                // connection; reconnection re-synchronises state.
                tracing::error!(
                    code = ?err.code,
                    detail = %err.detail,
                    "Switchboard reported a protocol error; expecting connection close."
                );
            }
        }
    }

    async fn command(&self, command: CoordCommand) {
        if self.commands.send(command).await.is_err() {
            tracing::error!("Supervisor stopped accepting coordinator commands.");
        }
    }
}

impl Inner {
    // This function returns `Ok(())` when a shutdown was explicitly requested,
    // and an error otherwise. Reconnection must be handled externally.
    async fn run(self: &Arc<Self>) -> Result<(), ()> {
        let (mut socket, server_hello) = match self.connect().await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to connect: {e}");
                return Err(());
            }
        };
        let mut update_rx = self.update_rx.lock().await;

        // No special on-connection behaviour is necessary: the switchboard will request the
        // supervisor status, and use that to determine if the information it has on file for this
        // supervisor's current job state is correct, which falls under the normal request handling
        // flow.

        tracing::info!("Received switchboard handshake: {server_hello:?}");

        let mut ping_interval = tokio::time::interval(self.config.ping_interval);
        let pong_timeout = tokio::time::sleep(self.config.pong_timeout);
        tokio::pin!(pong_timeout);

        // Clone the shutdown channel for us to be able to mutably borrow it:
        let mut shutdown_rx = self.shutdown_rx.clone();

        // The handle to the connector might be dropped before self (`Inner`,
        // which we hold a strong reference to). This will cause errors trying
        // to observe the shutdown channel. Once such an error occurred, avoid
        // polling it again.
        let mut shutdown_channel_dropped = false;

        loop {
            if !shutdown_channel_dropped && *shutdown_rx.borrow() && self.is_idle().await {
                tracing::info!(
                    "Supervisor connector is idle and shutdown was requested, exiting run() loop."
                );
                return Ok(());
            }

            #[rustfmt::skip]
            tokio::select! {
                changed = shutdown_rx.changed(), if !shutdown_channel_dropped => {
                    if let Err(recv_error) = changed {
                        warn!("Supervisor connector shutdown channel was closed, it will be impossible to shutdown the supervisor: {:?}", recv_error);
                        shutdown_channel_dropped = true;
                    }
                },

                _ = ping_interval.tick() =>  {
                    tracing::trace!(target: "tml_ws_connector:ping", "ping send tick");
                    if let Err(e) = socket.send(tungstenite::Message::Ping((&[][..]).into())).await {
                        tracing::error!("Failed to send PING message to switchboard, exiting socket control loop: {e:?}");
                        return Err(());
                    }
                }

                () = &mut pong_timeout => {
                    tracing::error!(
                        "Haven't received a PONG from switchboard in {:?}, exiting socket control loop",
                        self.config.pong_timeout
                    );
                    return Err(());
                }

                msg = update_rx.recv() => {
                    let msg = msg.unwrap();
                    let stringified = serde_json::to_string(&msg).unwrap();

                    tracing::debug!("Sending message: {msg:?}");

                    if let Err(e) = socket.send(tungstenite::Message::Text(stringified.into())).await {
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
                            return Err(());
                        }
                    };

                    match websocket_message {
                        tungstenite::Message::Text(s) => {
                            tracing::debug!("Received text message from websocket: {s}");
                            let msg = match serde_json::from_str::<SwitchboardToSupervisor>(&s) {
                                Ok(m) => m,
                                Err(e) => {
                                    tracing::error!("Failed to deserialize message: {e}");
                                    continue
                                }
                            };
                            // This is the reason we have separate WsConnector and Inner
                            // types: the supervisor holds a <dyn SupervisorConnector>, so
                            // we need something that is object-safe; however, to detach the
                            // wait for a command acknowledgement, `handle` needs to
                            // tokio::spawn, and for lifetime reasons self must then be
                            // 'static.
                            // However, to be object-safe, it won't work for
                            // SupervisorConnector::run to take self:&Arc<Self>; therefore
                            // we have an interior type that lives inside an Arc.
                            self.handle(msg).await;
                        }
                        tungstenite::Message::Binary(_) => {
                            tracing::error!("Received binary message from switchboard");
                        }
                        tungstenite::Message::Ping(_) => {
                            tracing::trace!(target: "tml_ws_connector:ping", "Received PING from switchboard");
                        }
                        tungstenite::Message::Pong(_) => {
                            pong_timeout
                                .as_mut()
                                .reset(Instant::now() + self.config.pong_timeout);
                            tracing::trace!("Received PONG from switchboard");
                        }
                        tungstenite::Message::Close(cf) => {
                            if let Some(cf) = cf {
                                tracing::warn!("Received close message; code = {}, reason = {}", cf.code, cf.reason);
                            } else {
                                tracing::warn!("Received close message with no close frame");
                            }
                            return Err(());
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

    /// Whether the supervisor is holding a job, asked of the supervisor itself.
    /// A supervisor that no longer answers cannot be holding one.
    async fn is_idle(&self) -> bool {
        let (reply, replied) = oneshot::channel();
        self.command(CoordCommand::StatusRequest { reply }).await;
        !matches!(
            replied.await,
            Ok(ReportedSupervisorStatus::HoldingJob { .. })
        )
    }

    async fn update_job_state(&self, job_id: Uuid, job_state: RunningJobState) {
        tracing::info!(
            "Supervisor provides job state for job {}: {:#?}",
            job_id,
            job_state
        );
        // Send the update to the run() loop, which will forward it to the switchboard
        if let Err(e) = self
            .update_tx
            .send(SupervisorToSwitchboard::SupervisorEvent(
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
        outcome: TaskExitStatus,
        message: Option<String>,
    ) {
        tracing::info!(
            "Supervisor provides exit status: job {}, status {:#?}",
            job_id,
            outcome
        );
        if let Err(e) = self
            .update_tx
            .send(SupervisorToSwitchboard::SupervisorEvent(
                SupervisorEvent::JobEvent {
                    job_id,
                    event: SupervisorJobEvent::DeclareExitStatus { outcome, message },
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
        // Send the error to the run() loop, which will forward it to the switchboard
        if let Err(e) = self
            .update_tx
            .send(SupervisorToSwitchboard::SupervisorEvent(
                SupervisorEvent::JobEvent {
                    job_id,
                    event: SupervisorJobEvent::Error { error },
                },
            ))
        {
            tracing::error!("failed to report job error to runloop: {e}")
        }
    }

    async fn report_job_network_address(&self, job_id: Uuid, address: IpAddr) {
        tracing::info!(
            "Supervisor provides job network address: job {}, address {}",
            job_id,
            address
        );
        if let Err(e) = self
            .update_tx
            .send(SupervisorToSwitchboard::SupervisorEvent(
                SupervisorEvent::JobEvent {
                    job_id,
                    event: SupervisorJobEvent::JobNetworkAddress { address },
                },
            ))
        {
            tracing::error!("failed to send job network address to runloop: {e}")
        }
    }

    async fn report_job_service_set(&self, job_id: Uuid, services: Vec<JobService>) {
        tracing::info!(
            "Supervisor provides job services: job {}, services {:#?}",
            job_id,
            services
        );
        if let Err(e) = self
            .update_tx
            .send(SupervisorToSwitchboard::SupervisorEvent(
                SupervisorEvent::JobEvent {
                    job_id,
                    event: SupervisorJobEvent::JobServiceSet { services },
                },
            ))
        {
            tracing::error!("failed to send job services to runloop: {e}")
        }
    }
}

use crate::audit::feed::{AuditFeedResponse, fetch_events_for_entity};
use axum::Json;
use std::net::SocketAddr;

/// Axum handler for the `/hosts/{id}/events` path.
pub async fn list_events(
    State(state): State<AppState>,
    subject: crate::auth::Subject,
    Path(host_id): Path<Uuid>,
) -> Result<Json<AuditFeedResponse>, StatusCode> {
    fetch_events_for_entity(&state, &subject, "host", host_id)
        .await
        .map(Json)
}

use axum::extract::{ConnectInfo, WebSocketUpgrade, ws};
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum_extra::TypedHeader;

use headers::Authorization;
use headers::authorization::Bearer;

use http::{HeaderMap, HeaderValue, StatusCode};

use tracing::instrument;

use treadmill_rs::api::switchboard_supervisor::websocket::{
    TREADMILL_PROTOCOL_MINOR_HEADER, TREADMILL_WEBSOCKET_CONFIG, TREADMILL_WEBSOCKET_PROTOCOL,
};
use treadmill_rs::api::switchboard_supervisor::{ProtocolVersion, ServerHello};

use uuid::Uuid;

use crate::auth::token::SecurityToken;
use crate::serve::AppState;
use crate::sql;
use crate::supervisor_ws_worker::{SupervisorWSWorker, SupervisorWSWorkerConfig};

// -- connect

/// Axum handler for the `/hosts/{id}/connect` path.
///
/// Responds with an `Upgrade: websocket` and launches the supervisor worker as
/// a `tokio` task.
#[instrument(skip(state))]
pub async fn connect(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    ConnectInfo(socket_addr): ConnectInfo<SocketAddr>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
    Path(host_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let auth_token = match SecurityToken::try_from(bearer) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("Failed to extract bearer token: {e}");
            return StatusCode::FORBIDDEN.into_response();
        }
    };

    let auth_result = sql::host::try_authenticate_for_host(host_id, auth_token, state.pool()).await;
    match auth_result {
        Ok(true) => (), // Success!
        Ok(false) => {
            tracing::warn!("invalid host-token ({host_id}, {auth_token}) combination");
            return StatusCode::FORBIDDEN.into_response();
        }
        Err(e) => {
            tracing::error!(
                "failed to authenticate supervisor for host ({host_id}) with token ({auth_token}): {e}"
            );
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    /// Check that the WebSocket subprotocol is correctly specified as `treadmill`.
    fn check_protocol_header(protocol: Option<&HeaderValue>, socket_addr: SocketAddr) -> bool {
        if let Some(protocol) = protocol {
            if protocol != HeaderValue::from_static(TREADMILL_WEBSOCKET_PROTOCOL) {
                tracing::error!(
                    "Websocket connection from {socket_addr} specifies \
		     `Sec-Websocket-Protocol: {protocol:?}`, which is not \
		     recognized. Closing."
                );
                false
            } else {
                true
            }
        } else {
            tracing::error!(
                "Websocket connection from {socket_addr} does not specify \
		 Sec-Websocket-Protocol, closing."
            );
            false
        }
    }

    // Minor-version negotiation: the supervisor advertises its protocol minor
    // in a request header (absent ⇒ treat as 0, i.e. an older peer). The
    // effective minor for this connection is the lower of the two; neither side
    // may emit a feature/variant introduced above it.
    let supervisor_minor: u16 = headers
        .get(TREADMILL_PROTOCOL_MINOR_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // `min` is a no-op while PROTOCOL_MINOR is 0, but it is the correct
    // negotiation once minors diverge; keep it rather than hard-code 0.
    #[allow(clippy::unnecessary_min_or_max)]
    let effective_minor = supervisor_minor.min(ProtocolVersion::CURRENT.minor);
    tracing::info!(
        supervisor_minor,
        server_minor = ProtocolVersion::CURRENT.minor,
        effective_minor,
        "Negotiated protocol minor with supervisor for host ({host_id})."
    );

    let worker_pool = state.pool().clone();

    let ws_worker_config = SupervisorWSWorkerConfig {
        supervisor_ping_interval: state.config().service.supervisor_ping_interval,
        supervisor_pong_dead: state.config().service.supervisor_pong_dead,
        supervisor_reconcile_interval: state.config().service.supervisor_reconcile_interval,
    };

    // Shared with the worker so it can mint per-job write tokens and provision
    // streams at dispatch; `None` when log streaming is disabled.
    let log_streaming = state.log_streaming().cloned();

    let mut response = ws.protocols([TREADMILL_WEBSOCKET_PROTOCOL]).on_upgrade(
        move |mut web_socket| async move {
            tokio::spawn(async move {
                // Resolve the subprotocol check into an owned `bool` before
                // any `.await`: `protocol()` borrows `web_socket`, and that
                // borrow must not straddle the send below — a live
                // `&WebSocket` across an await would force the spawned future
                // to require `WebSocket: Sync`, which it isn't.
                let wrong_protocol = !check_protocol_header(web_socket.protocol(), socket_addr);
                if wrong_protocol {
                    if let Err(e) = web_socket.send(ws::Message::Close(None)).await {
                        tracing::error!(
                            "Failed to send close frame (wrong subprotocol) to {socket_addr}: {e}."
                        );
                    }
                    return;
                }

                tracing::info!(
                    "Starting SupervisorWSWorker for host \
		     ({host_id}), connecting from {socket_addr}."
                );

                SupervisorWSWorker::run(
                    worker_pool,
                    host_id,
                    web_socket,
                    ws_worker_config,
                    log_streaming,
                )
                .await
            });
        },
    );

    let server_hello_json = serde_json::to_string(&ServerHello {
        protocol: ProtocolVersion::CURRENT,
        features: Default::default(),
    })
    .expect("Failed to serialize ServerHello");
    response.headers_mut().insert(
        TREADMILL_WEBSOCKET_CONFIG,
        server_hello_json
            .parse()
            .expect("Failed to parse serialized socket configuration into HTTP header value"),
    );

    response
}

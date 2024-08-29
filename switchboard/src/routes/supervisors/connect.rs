use crate::auth::token::SecurityToken;
use crate::serve::AppState;
use crate::sql;
use axum::extract::{ws, ConnectInfo, Path, State, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use axum_extra::TypedHeader;
use headers::authorization::Bearer;
use headers::Authorization;
use http::{HeaderValue, StatusCode};
use std::net::SocketAddr;
use tracing::instrument;
use treadmill_rs::api::switchboard_supervisor::ws_challenge::TREADMILL_WEBSOCKET_PROTOCOL;
use uuid::Uuid;

/// Axum handler for the `/supervisor/:id/connect` path.
///
/// Responds with an `Upgrade: websocket` and launches [`launch_supervisor_actor`] as a `tokio` task.
#[instrument(skip(state))]
pub async fn connect(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    ConnectInfo(socket_addr): ConnectInfo<SocketAddr>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
    Path(supervisor_id): Path<Uuid>,
) -> Response {
    let auth_token = match SecurityToken::try_from(bearer) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("Failed to extract bearer token: {e}");
            return StatusCode::FORBIDDEN.into_response();
        }
    };
    let auth_result =
        sql::supervisor::try_authenticate_supervisor(supervisor_id, auth_token, state.pool()).await;
    match auth_result {
        Ok(b) => {
            if b {
                //
            } else {
                tracing::warn!(
                    "invalid supervisor-token ({supervisor_id}, {auth_token}) combination"
                );
                return StatusCode::FORBIDDEN.into_response();
            }
        }
        Err(e) => {
            tracing::error!("failed to authenticate supervisor ({supervisor_id}) with token ({auth_token}): {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    /// Check that the WebSocket subprotocol is correctly specified as `treadmill`.
    fn check_protocol_header(protocol: Option<&HeaderValue>, socket_addr: SocketAddr) -> bool {
        if let Some(protocol) = protocol {
            if protocol != HeaderValue::from_static(TREADMILL_WEBSOCKET_PROTOCOL) {
                let protocol_str = protocol.to_str().unwrap_or_else(|e| {
                    tracing::error!("Websocket connection from {socket_addr} specifies Sec-Websocket-Protocol that cannot be converted to a string: {e}, closing.");
                    "<invalid>"
                });
                tracing::error!("Websocket connection from {socket_addr} specifies `Sec-Websocket-Protocol: {protocol_str}`, which is not recognized. Closing.");
            } else {
                return true;
            }
        } else {
            tracing::error!("Websocket connection from {socket_addr} does not specify Sec-Websocket-Protocol, closing.");
        }
        false
    }

    ws.protocols([TREADMILL_WEBSOCKET_PROTOCOL])
        .on_upgrade(move |mut web_socket| async move {
            tokio::spawn(async move {
                let maybe_subprotocol = web_socket.protocol();
                if !check_protocol_header(maybe_subprotocol, socket_addr) {
                    if let Err(e) = web_socket.send(ws::Message::Close(None)).await {
                        tracing::error!(
                            "Failed to send close frame (wrong subprotocol) to {socket_addr}: {e}."
                        );
                        return;
                    }
                }

                tracing::info!("Supervisor ({supervisor_id}) connecting from {socket_addr}.");

                if let Err(e) = state
                    .service()
                    .supervisor_connected(supervisor_id, web_socket)
                    .await
                {
                    tracing::error!("Failed to connect supervisor ({supervisor_id}): {e}");
                }
            });
        })
}

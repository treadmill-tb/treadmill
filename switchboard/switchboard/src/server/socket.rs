//! Server side handling for supervisor-switchboard websocket connections.

use crate::server::socket::auth::{authenticate_supervisor, AuthenticationResult};
use crate::server::AppState;
use axum::extract;
use axum::extract::connect_info::ConnectInfo;
use axum::extract::ws::{CloseFrame, WebSocket};
use axum::extract::{ws, WebSocketUpgrade};
use axum::response::Response;
use std::net::SocketAddr;

mod auth;

/// Axum handler for the `/supervisor` path.
///
/// Responds with an `Upgrade: websocket` and launches [`launch_supervisor_actor`] as a `tokio` task.
#[tracing::instrument]
pub async fn supervisor_handler(
    ws: WebSocketUpgrade,
    extract::State(state): extract::State<AppState>,
    ConnectInfo(socket_addr): ConnectInfo<SocketAddr>,
) -> Response {
    ws.protocols(["treadmill"])
        .on_upgrade(move |web_socket| async move {
            tokio::spawn(
                async move { launch_supervisor_actor(web_socket, state, socket_addr).await },
            );
        })
}

/// Check that the WebSocket subprotocol is correctly specified as `treadmill`.
fn check_protocol_header(protocol: Option<&http::HeaderValue>, socket_addr: SocketAddr) -> bool {
    if let Some(protocol) = protocol {
        if protocol != http::HeaderValue::from_static("treadmill") {
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

/// Launch the actor representing a particular supervisor.
async fn launch_supervisor_actor(mut socket: WebSocket, state: AppState, socket_addr: SocketAddr) {
    /// Helper function to close a socket.
    async fn try_close(
        mut socket: WebSocket,
        socket_addr: SocketAddr,
        maybe_cf: Option<CloseFrame<'static>>,
    ) {
        if let Err(e) = socket.send(ws::Message::Close(maybe_cf)).await {
            tracing::error!("Failed to send close frame to {socket_addr}: {e}.");
        }
        if let Err(e) = socket.close().await {
            tracing::error!("Failed to close websocket connection from {socket_addr}: {e}.");
        }
    }

    // -- Check that the subprotocol is correct.

    if !check_protocol_header(socket.protocol(), socket_addr) {
        try_close(socket, socket_addr, None).await;
        return;
    }

    // -- Authenticate the supervisor.

    let auth_message_timeout = state.config.websocket.auth.per_message_timeout;
    let supervisor_id =
        match authenticate_supervisor(&mut socket, &state, socket_addr, auth_message_timeout).await
        {
            Ok(auth_result) => match auth_result {
                AuthenticationResult::Authenticated { as_supervisor_id } => as_supervisor_id,
                AuthenticationResult::Unauthenticated => {
                    tracing::error!(
                        "Closing unauthenticated websocket connection from {socket_addr}."
                    );
                    try_close(socket, socket_addr, None).await;
                    return;
                }
            },
            Err(e) => {
                tracing::error!(
                    "Failed to authenticate websocket connection from {socket_addr}: {e}. Closing."
                );
                try_close(socket, socket_addr, None).await;
                return;
            }
        };

    tracing::info!("Supervisor ({supervisor_id}) has connected from {socket_addr}");

    // -- Connection is OK, run the actor loop

    // TODO: Actor goes here

    // -- Main loop has exited, close the connection

    try_close(
        socket,
        socket_addr,
        Some(CloseFrame {
            code: ws::close_code::NORMAL,
            reason: "terminated".into(),
        }),
    )
    .await;
}

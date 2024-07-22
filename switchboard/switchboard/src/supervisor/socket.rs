//! Server side handling for supervisor-switchboard websocket connections.

use crate::{
    model,
    server::AppState,
    supervisor::auth::{authenticate_supervisor, AuthenticationResult},
};
use axum::{
    extract::{
        self,
        connect_info::ConnectInfo,
        ws::{CloseFrame, Message as WsMessage, WebSocket, WebSocketUpgrade},
    },
    response::Response,
};
use std::net::SocketAddr;
use treadmill_rs::api::switchboard_supervisor::ws_challenge::TREADMILL_WEBSOCKET_PROTOCOL;

/// Axum handler for the `/supervisor` path.
///
/// Responds with an `Upgrade: websocket` and launches [`launch_supervisor_actor`] as a `tokio` task.
#[tracing::instrument]
pub async fn supervisor_handler(
    ws: WebSocketUpgrade,
    extract::State(state): extract::State<AppState>,
    ConnectInfo(socket_addr): ConnectInfo<SocketAddr>,
) -> Response {
    ws.protocols([TREADMILL_WEBSOCKET_PROTOCOL])
        .on_upgrade(move |web_socket| async move {
            tokio::spawn(
                async move { launch_supervisor_actor(web_socket, state, socket_addr).await },
            );
        })
}

/// Check that the WebSocket subprotocol is correctly specified as `treadmill`.
fn check_protocol_header(protocol: Option<&http::HeaderValue>, socket_addr: SocketAddr) -> bool {
    if let Some(protocol) = protocol {
        if protocol != http::HeaderValue::from_static(TREADMILL_WEBSOCKET_PROTOCOL) {
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
        if let Err(e) = socket.send(WsMessage::Close(maybe_cf)).await {
            tracing::error!("Failed to send close frame to {socket_addr}: {e}.");
        }
        // .send(..::Close(..)) already closes the socket, so no need to call .close()
    }

    // -- Check that the subprotocol is correct.

    if !check_protocol_header(socket.protocol(), socket_addr) {
        try_close(socket, socket_addr, None).await;
        return;
    }

    // -- Authenticate the supervisor.

    let auth_message_timeout = state.config().websocket.auth.per_message_timeout;
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
    let model = model::supervisor::fetch_by_id(supervisor_id, state.pool())
        .await
        .expect("supervisor was deleted from database between authentication and model lookup");
    state.herd().add_supervisor(model, socket).await;

    // // temporary test:
    // socket
    //     .send(ws::Message::Text(
    //         serde_json::to_string(&switchboard_supervisor::Message::StartJob(
    //             StartJobMessage {
    //                 job_id: Uuid::new_v4(),
    //                 ssh_keys: vec![],
    //                 restart_policy: RestartPolicy {
    //                     remaining_restart_count: 0,
    //                 },
    //                 ssh_rendezvous_servers: vec![],
    //                 parameters: Default::default(),
    //                 init_spec: JobInitSpec::Image {
    //                     image_id: ImageId(rand::random()),
    //                 },
    //             },
    //         ))
    //         .unwrap(),
    //     ))
    //     .await
    //     .unwrap();

    // let () = std::future::pending().await;
    // //
    // // -- Main loop has exited, close the connection
    //
    // tracing::info!("Closing connection with supervisor ({supervisor_id})");
    //
    // try_close(
    //     socket,
    //     socket_addr,
    //     Some(CloseFrame {
    //         code: ws::close_code::NORMAL,
    //         reason: "terminated".into(),
    //     }),
    // )
    // .await;
}

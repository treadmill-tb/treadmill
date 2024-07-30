//! Server side handling for supervisor-switchboard websocket connections.

use crate::model;
use crate::server::token::SecurityToken;
use crate::server::AppState;
use axum::response::IntoResponse;
use axum::{
    extract::{
        self,
        connect_info::ConnectInfo,
        ws::{CloseFrame, Message as WsMessage, WebSocket, WebSocketUpgrade},
    },
    response::Response,
};
use axum_extra::TypedHeader;
use headers::authorization::Bearer;
use headers::Authorization;
use http::StatusCode;
use sqlx::PgExecutor;
use std::net::SocketAddr;
use subtle::ConstantTimeEq;
use treadmill_rs::api::switchboard_supervisor::ws_challenge::TREADMILL_WEBSOCKET_PROTOCOL;
use uuid::Uuid;

/// Axum handler for the `/supervisor` path.
///
/// Responds with an `Upgrade: websocket` and launches [`launch_supervisor_actor`] as a `tokio` task.
#[tracing::instrument]
pub async fn supervisor_handler(
    ws: WebSocketUpgrade,
    extract::State(state): extract::State<AppState>,
    ConnectInfo(socket_addr): ConnectInfo<SocketAddr>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
    extract::Path(supervisor_id): extract::Path<Uuid>,
) -> Response {
    let auth_token = match SecurityToken::try_from(bearer) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("Failed to extract bearer token: {e}");
            return StatusCode::FORBIDDEN.into_response();
        }
    };
    match try_authenticate(supervisor_id, auth_token, state.pool()).await {
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
    ws.protocols([TREADMILL_WEBSOCKET_PROTOCOL])
        .on_upgrade(move |web_socket| async move {
            tokio::spawn(async move {
                launch_supervisor_actor(web_socket, supervisor_id, state, socket_addr).await
            });
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
async fn launch_supervisor_actor(
    socket: WebSocket,
    supervisor_id: Uuid,
    state: AppState,
    socket_addr: SocketAddr,
) {
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

    // -- Connection is OK, run the actor loop

    tracing::info!("Supervisor ({supervisor_id}) has connected from {socket_addr}");

    // TODO: Actor goes here
    // TODO: error handling
    let model = model::supervisor::fetch_by_id(supervisor_id, state.pool())
        .await
        .expect("supervisor was deleted from database between authentication and model lookup");
    state.herd().add_supervisor(&model, socket).await;
}

pub async fn try_authenticate(
    supervisor_id: Uuid,
    auth_token: SecurityToken,
    db: impl PgExecutor<'_>,
) -> Result<bool, sqlx::Error> {
    // how to do this without leaking timing?
    let q = sqlx::query!(
        r#"select supervisor_id, auth_token from supervisors where supervisor_id = $1 limit 1;"#,
        supervisor_id,
    )
    .fetch_one(db)
    .await;
    let v = q
        .as_ref()
        .map(|r| r.auth_token.clone())
        .unwrap_or(vec![0u8; 128]);
    let st = SecurityToken::try_from(v).expect("stored auth token in database is invalid");
    Ok(bool::from(
        st.ct_eq(&auth_token) & ({ q.is_ok() as u8 }.ct_eq(&1)),
    ))
}

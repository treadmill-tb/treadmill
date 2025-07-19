use std::net::SocketAddr;

use axum::extract::Query;
use axum::extract::{ConnectInfo, WebSocketUpgrade, ws};
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum_extra::TypedHeader;

use futures_util::StreamExt;
use futures_util::stream::FuturesOrdered;

use headers::Authorization;
use headers::authorization::Bearer;

use http::{HeaderValue, StatusCode};

use tracing::instrument;

use treadmill_rs::api::switchboard::supervisors::list::{Filter, Response as LSResponse};
use treadmill_rs::api::switchboard::supervisors::status::Response as SSResponse;
use treadmill_rs::api::switchboard_supervisor::websocket::{
    TREADMILL_WEBSOCKET_CONFIG, TREADMILL_WEBSOCKET_PROTOCOL,
};

use uuid::Uuid;

use crate::auth::AuthorizationSource;
use crate::auth::db::DbAuth;
use crate::auth::extract::AuthSource;
use crate::auth::token::SecurityToken;
use crate::perms::read_supervisor_status;
use crate::routes::proxy::{Proxied, proxy_err, proxy_val};
use crate::serve::AppState;
use crate::sql;
use crate::supervisor_ws_worker::SupervisorWSWorker;
use crate::{impl_from_auth_err, perms};

// -- status

impl_from_auth_err!(SSResponse, Database => Internal, Unauthorized => Invalid);
#[tracing::instrument(skip(state, auth))]
pub async fn status(
    State(state): State<AppState>,
    AuthSource(auth): AuthSource<DbAuth>,
    Path(supervisor_id): Path<Uuid>,
) -> Proxied<SSResponse> {
    let access = auth
        .authorize(perms::ReadSupervisorStatus { supervisor_id })
        .await
        .map_err(proxy_err)?;
    let supervisor_status = read_supervisor_status(&state, access)
        .await
        .map_err(|_| proxy_err(SSResponse::Invalid))?;

    proxy_val(SSResponse::Ok {
        status: supervisor_status,
    })
}

// -- list

impl_from_auth_err!(LSResponse, Database => Internal, Unauthorized => Unauthorized);
#[tracing::instrument(skip(state, auth))]
pub async fn list(
    State(state): State<AppState>,
    AuthSource(auth): AuthSource<DbAuth>,
    Query(filter): Query<Filter>,
) -> Proxied<LSResponse> {
    let list_supervisors = auth
        .authorize(perms::ListSupervisors { filter })
        .await
        .map_err(proxy_err)?;

    todo!();

    // let perm_queries: FuturesOrdered<_> = state
    //     .service()
    //     .list_supervisors()
    //     .await
    //     .into_iter()
    //     .map(|supervisor_id| auth.authorize(perms::ReadSupervisorStatus { supervisor_id }))
    //     .collect();
    // let supervisor_perms: Vec<_> = perm_queries
    //     .filter_map(|x| async move { x.ok() })
    //     .collect()
    //     .await;
    // let supervisors = perms::list_supervisors(&state, list_supervisors, supervisor_perms).await;
    // proxy_val(LSResponse::Ok { supervisors })
}

// -- connect

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
        Ok(true) => (), // Success!
        Ok(false) => {
            tracing::warn!("invalid supervisor-token ({supervisor_id}, {auth_token}) combination");
            return StatusCode::FORBIDDEN.into_response();
        }
        Err(e) => {
            tracing::error!(
                "failed to authenticate supervisor ({supervisor_id}) with token ({auth_token}): {e}"
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

    let worker_state = state.clone();
    let mut response = ws.protocols([TREADMILL_WEBSOCKET_PROTOCOL]).on_upgrade(
        move |mut web_socket| async move {
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

                tracing::info!(
                    "Starting SupervisorWSWorker for supervisor \
		     ({supervisor_id}), connecting from {socket_addr}."
                );

                SupervisorWSWorker::run(worker_state, supervisor_id, web_socket).await
            });
        },
    );

    let socket_config_json = serde_json::to_string(&state.config().service.socket)
        .expect("Failed to serialize socket configuration");
    response.headers_mut().insert(
        TREADMILL_WEBSOCKET_CONFIG,
        socket_config_json
            .parse()
            .expect("Failed to parse serialized socket configuration into HTTP header value"),
    );

    response
}

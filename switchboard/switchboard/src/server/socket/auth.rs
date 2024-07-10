//! Handles authentication of incoming websocket connections that claim to be supervisors.
//!
//! # Authentication process
//!
//! The authentication is challenge based; the switchboard sends the supervisor a randomly generated
//! nonce, which the supervisor then signs with an ed25519 signing key (private key). The supervisor
//! then sends the signature back, and the switchboard uses the supervisor's stored verifying key
//! (public key) in the database to verify the supervisor.
//!
//! (See [`crate::schemas::switchboard_supervisor::challenge`] for the relevant types)

use crate::{
    schemas::switchboard_supervisor::challenge::{
        Challenge, ChallengeMessage, ChallengeRequest, ChallengeResponse, ChallengeResult,
        NONCE_LEN,
    },
    server::AppState,
};
use axum::extract::ws::{Message, WebSocket};
use ed25519_dalek::pkcs8::DecodePublicKey;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use futures_util::StreamExt;
use rand::{RngCore, SeedableRng};
use std::cell::RefCell;
use std::net::SocketAddr;
use std::time::Duration;
use thiserror::Error;
use uuid::Uuid;

/// Errors that may occur during the authentication process.
#[derive(Debug, Error)]
pub enum AuthError {
    /// Failed to send a message over the websocket.
    #[error("failed to send message")]
    Send(axum::Error),
    /// Failed to receive a message from the websocket.
    #[error("failed to receive message")]
    Receive(axum::Error),
    /// Failed to serialize an outgoing message.
    #[error("failed to serialize message")]
    Serialization(#[source] serde_json::Error),
    /// Failed to deserialize an incoming message.
    #[error("failed to deserialize message")]
    Deserialization(#[source] serde_json::Error),
    /// Received an invalid message.
    #[error("received invalid message for current context")]
    InvalidMessage,
    /// The key stored in the database is invalid or otherwise could not be used.
    #[error("stored key is invalid")]
    InvalidStoredKey,
    /// An error occur while trying to query the database.
    #[error("failed to authenticate socket: database error")]
    Database(#[source] sqlx::Error),
    /// The connection closed unexpectedly.
    #[error("connection closed unexpectedly")]
    Closed,
    /// Server timed out while waiting for client message.
    #[error("timed out waiting for message from client")]
    Timeout,
}

/// The result of an authentication challenge.
/// Note that since the (putative) identity of the supervisor is established at the start of the
/// challenge process, in order for the main supervisor control system to get this identity
/// information, we need to pass it through the authentication result here.
#[derive(Debug)]
pub enum AuthenticationResult {
    /// The client has successfully authenticated as the supervisor whose UUID is given in
    /// `as_supervisor_id`.
    Authenticated { as_supervisor_id: Uuid },
    /// The client failed to authenticate.
    Unauthenticated,
}

/// Attempt to authenticate a websocket client that is claiming to be a supervisor.
///
/// At the moment `remote_addr` is only used for logging purposes.
pub async fn authenticate_supervisor(
    socket: &mut WebSocket,
    app_state: &AppState,
    remote_addr: SocketAddr,
    per_message_timeout: Duration,
) -> Result<AuthenticationResult, AuthError> {
    let mut machine = SocketAuthenticator {
        state: AuthState::AwaitChallengeRequest,
    };
    while let Some(r) = tokio::time::timeout(per_message_timeout, socket.next())
        .await
        .map_err(|_| AuthError::Timeout)?
    {
        match r {
            Ok(ws_message) => match ws_message {
                Message::Text(s) => {
                    let m: ChallengeMessage =
                        serde_json::from_str(&s).map_err(AuthError::Deserialization)?;
                    if let Some(result) = machine
                        .handle_challenge_message(socket, app_state, remote_addr, m)
                        .await?
                    {
                        return Ok(result);
                    }
                }
                Message::Binary(_) => {
                    tracing::error!(
                        "Received binary message from {remote_addr} during authentication"
                    );
                    return Err(AuthError::InvalidMessage);
                }
                // TODO: Test if ping/pong are actually handled automatically.
                //       I've had persistent issues with "connection reset by peer" on a database
                //       project that I never actually got to the root of.
                Message::Ping(_) => {
                    // Should be handled automatically?
                    tracing::trace!("websocket ping'ed");
                }
                Message::Pong(_) => {
                    // Should be handled automatically?
                    tracing::trace!("websocket pong'ed");
                }
                Message::Close(maybe_cf) => {
                    let cm = maybe_cf
                        .map(|cf| format!("({}) {}", cf.code, cf.reason))
                        .unwrap_or("<no close frame>".to_string());
                    tracing::error!(
                        "Received close message from {remote_addr} during authentication: {cm}"
                    );
                    return Err(AuthError::Closed);
                }
            },
            Err(e) => {
                tracing::error!("Error receiving from {remote_addr} during authentication: {e}");
                return Err(AuthError::Receive(e));
            }
        }
    }
    tracing::error!("Connection from {remote_addr} closed unexpectedly during authentication");
    return Err(AuthError::Closed);
}

// INTERNAL TYPES ----------------------------------------------------------------------------------

/// Internal state of the authentication state machine
#[derive(Debug)]
enum AuthState {
    /// Waiting for client to issue challenge request
    AwaitChallengeRequest,
    /// Waiting for client to issue challenge response
    AwaitChallengeResponse {
        client_uuid: Uuid,
        nonce: [u8; NONCE_LEN],
    },
    /// Authentication process finished
    Finished,
}

thread_local! {
    /// Used to generate nonces to be signed by the client.
    static THREAD_LOCAL_NONCE_RNG : RefCell<rand_chacha::ChaCha20Rng> = RefCell::new(rand_chacha::ChaCha20Rng::from_entropy());
}

/// State machine responsible for authenticating incoming websocket connections from supervisors.
///
/// State Diagram:
/// ```txt
///                             ┌── Network Boundary
/// ┌───────────────────────┐   │
/// │ AwaitChallengeRequest │   │
/// └───────────┬───────────┘   │
///             │               │
///             │◄───────────── │ ── ChallengeRequest
///             │               │
///             │               │
///             ├────────────── │ ─► Challenge
///             │               │
/// ┌───────────▼────────────┐  │
/// │ AwaitChallengeResponse │  │
/// └───────────┬────────────┘  │
///             │               │
///             │◄───────────── │ ── ChallengeResponse
///             │               │
///             │               │
///             ├────────────── │ ─► ChallengeResult
///             │               │
///       ┌─────▼────┐          │
///       │ Finished │          │
///       └──────────┘          │
/// ```
#[derive(Debug)]
struct SocketAuthenticator {
    state: AuthState,
}

impl SocketAuthenticator {
    #[tracing::instrument(skip(socket))]
    async fn handle_challenge_message(
        &mut self,
        socket: &mut WebSocket,
        app_state: &AppState,
        remote_addr: SocketAddr,
        message: ChallengeMessage,
    ) -> Result<Option<AuthenticationResult>, AuthError> {
        match (&self.state, message) {
            // CASE 1 -- Client asked for an authentication challenge, so we send one.
            (
                AuthState::AwaitChallengeRequest,
                ChallengeMessage::ChallengeRequest(ChallengeRequest { uuid }),
            ) => {
                let nonce = self.issue_challenge(socket, uuid, remote_addr).await?;
                self.state = AuthState::AwaitChallengeResponse {
                    client_uuid: uuid,
                    nonce,
                };
                Ok(None)
            }

            // CASE 2 -- Client responded to challenge, so we authenticate the response and send the
            // result.
            (
                AuthState::AwaitChallengeResponse { client_uuid, nonce },
                ChallengeMessage::ChallengeResponse(ChallengeResponse {
                    switchboard_nonce_signature,
                }),
            ) => {
                let status = self
                    .issue_challenge_response(
                        socket,
                        app_state,
                        client_uuid,
                        remote_addr,
                        nonce,
                        switchboard_nonce_signature,
                    )
                    .await?;
                let as_supervisor_id = *client_uuid;
                self.state = AuthState::Finished;
                Ok(Some(match status {
                    ChallengeResult::Authenticated => {
                        AuthenticationResult::Authenticated { as_supervisor_id }
                    }
                    ChallengeResult::Unauthenticated => AuthenticationResult::Unauthenticated,
                }))
            }

            // CASE 3 -- Literally anything else happens, and we close the connection.
            (_, _) => {
                tracing::error!("Received invalid message for current auth state");
                Err(AuthError::InvalidMessage)
            }
        }
    }

    async fn issue_challenge(
        &self,
        socket: &mut WebSocket,
        uuid: Uuid,
        remote_addr: SocketAddr,
    ) -> Result<[u8; NONCE_LEN], AuthError> {
        tracing::info!(
            "Received challenge request for supervisor ({uuid}) from remote {remote_addr}"
        );
        let mut switchboard_nonce = [0u8; NONCE_LEN];
        THREAD_LOCAL_NONCE_RNG.with(|m| {
            m.borrow_mut().fill_bytes(&mut switchboard_nonce);
        });
        let message = ChallengeMessage::Challenge(Challenge { switchboard_nonce });
        let serialized = serde_json::to_string(&message).map_err(AuthError::Serialization)?;
        let ws_message = Message::Text(serialized);
        socket.send(ws_message).await.map_err(AuthError::Send)?;

        Ok(switchboard_nonce)
    }

    async fn issue_challenge_response(
        &self,
        socket: &mut WebSocket,
        app_state: &AppState,
        uuid: &Uuid,
        remote_addr: SocketAddr,
        nonce: &[u8; NONCE_LEN],
        signature: Signature,
    ) -> Result<ChallengeResult, AuthError> {
        tracing::info!(
            "Received challenge response for supervisor ({uuid}) from remote {remote_addr}"
        );
        // Load verifying key: query database, parse from pkcs8
        struct PublicKeyRecord {
            public_key: String,
        }
        let res = sqlx::query_as!(
            PublicKeyRecord,
            "SELECT public_key FROM supervisors WHERE supervisor_id = $1",
            uuid
        )
        .fetch_one(&app_state.db_pool)
        .await;
        let public_key_record = match res {
            Ok(pkr) => pkr,
            Err(sqlx::Error::RowNotFound) => {
                tracing::warn!(
                    "No such supervisor: supervisor ({uuid}), as requested by {remote_addr}"
                );
                return Ok(ChallengeResult::Unauthenticated);
            }
            Err(e) => {
                tracing::error!("Failed to query verifying key for supervisor ({uuid}): {e}");
                return Err(AuthError::Database(e));
            }
        };

        let vkey = match VerifyingKey::from_public_key_pem(&public_key_record.public_key) {
            Ok(vkey) => vkey,
            Err(e) => {
                tracing::error!("Verifying key for supervisor ({uuid}) couldn't be loaded: {e}");
                return Err(AuthError::InvalidStoredKey);
            }
        };
        // Verify key
        let status = if let Err(e) = vkey.verify(nonce, &signature) {
            tracing::warn!(
                "Couldn't verify challenge response signature for supervisor ({uuid}) from {remote_addr}: {e}"
            );
            ChallengeResult::Unauthenticated
        } else {
            ChallengeResult::Authenticated
        };
        // Return result message
        let message = ChallengeMessage::ChallengeResult(status);
        let serialized = serde_json::to_string(&message).map_err(AuthError::Serialization)?;
        let ws_message = Message::Text(serialized);
        socket.send(ws_message).await.map_err(AuthError::Send)?;

        Ok(status)
    }
}

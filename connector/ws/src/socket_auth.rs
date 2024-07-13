//! Handles authentication for outgoing websocket connection.

use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use treadmill_rs::api::coord_supervisor::ws_challenge::*;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum AuthError {
    //
}

pub async fn authenticate_as_supervisor(
    socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    supervisor_id: Uuid,
    signing_key: SigningKey,
) -> Result<(), AuthError> {
    socket
        .send(Message::Text(
            serde_json::to_string(&ChallengeMessage::ChallengeRequest(ChallengeRequest {
                uuid: supervisor_id,
            }))
            .unwrap(),
        ))
        .await
        .unwrap();
    let m = socket.next().await.unwrap().unwrap();
    let Message::Text(s) = &m else {
        panic!("Non-Text Message: {m:?}")
    };
    let c: ChallengeMessage = serde_json::from_str(s).unwrap();
    let ChallengeMessage::Challenge(c) = c else {
        panic!("Not A Challenge")
    };
    let signature = signing_key.sign(&c.switchboard_nonce);
    socket
        .send(Message::Text(
            serde_json::to_string(&ChallengeMessage::ChallengeResponse(ChallengeResponse {
                switchboard_nonce_signature: signature,
            }))
            .unwrap(),
        ))
        .await
        .unwrap();
    let r = socket.next().await.unwrap();
    println!("result: {r:?}");
    Ok(())
}

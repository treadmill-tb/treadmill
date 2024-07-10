use base64::Engine;
use clap::Parser;
use ed25519_dalek::pkcs8::DecodePrivateKey;
use rand::{RngCore, SeedableRng};
use std::path::PathBuf;
use std::str::FromStr;
use tokio_tungstenite::Connector;
use uuid::Uuid;

#[derive(clap::Parser)]
#[command(version, about)]
struct Args {
    #[arg(long)]
    signing_key: PathBuf,
    uri: String,
    #[arg(long)]
    uuid: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let signing_key = ed25519_dalek::SigningKey::from_pkcs8_pem(
        std::fs::read_to_string(args.signing_key).unwrap().as_str(),
    )
    .unwrap();
    let uuid = Uuid::from_str(&args.uuid).unwrap();

    let mut key_buf = [0u8; 16];
    rand_chacha::ChaCha20Rng::from_entropy().fill_bytes(&mut key_buf);
    let base64_key = base64::prelude::BASE64_STANDARD.encode(&key_buf);
    let uri = tokio_tungstenite::tungstenite::http::Uri::from_str(&args.uri).unwrap();
    let req = tokio_tungstenite::tungstenite::http::Request::builder()
        .method("GET")
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", base64_key)
        .header("Sec-WebSocket-Protocol", "treadmill")
        .header("Host", uri.host().unwrap())
        .uri(uri)
        .body(())
        .unwrap();

    let (mut ws, _resp) =
        tokio_tungstenite::connect_async_tls_with_config(req, None, false, Some(Connector::Plain))
            .await
            .unwrap();

    tml_ws_connector::socket_auth::authenticate_as_supervisor(&mut ws, uuid, signing_key)
        .await
        .unwrap();
}

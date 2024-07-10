use tokio_tungstenite::Connector;
use uuid::Uuid;

pub mod socket_auth;

#[derive(Debug)]
pub struct Config {}

async fn connect(remote: String, id: Uuid) {
    // let request = tokio_tungstenite::tungstenite::http::Request::builder()
    //     .header("Sec-WebSocket-Protocol", "treadmill")
    //     .uri(remote)
    //     .body(())
    //     .unwrap();
    let (mut ws, _resp) = tokio_tungstenite::connect_async_tls_with_config(
        remote,
        None,
        false,
        Some(Connector::Plain),
    )
    .await
    .unwrap();
}

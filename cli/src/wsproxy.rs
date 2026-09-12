use anyhow::{Context as _, Result, bail};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{Connector, connect_async_tls_with_config};

use crate::ssh::SERVICE_TOKEN_ENV;

const READ_BUFFER: usize = 16 * 1024;

/// Bridge stdin/stdout to a job's `sshws` service: the gateway and the job's
/// own proxy both admit the request against the service token, and the socket
/// then carries raw SSH bytes in binary frames.
pub async fn run(hostname: &str, port: u16, insecure_tls: bool, verbose: u8) -> Result<()> {
    let token = std::env::var(SERVICE_TOKEN_ENV)
        .ok()
        .filter(|token| !token.is_empty())
        .with_context(|| {
            format!("{SERVICE_TOKEN_ENV} is not set; run this through `tml job ssh`")
        })?;

    let mut request = format!("wss://{hostname}:{port}/")
        .into_client_request()
        .context("building the WebSocket request")?;
    request.headers_mut().insert(
        "X-Tml-Token",
        HeaderValue::from_str(&token).context("the service token is not a valid header value")?,
    );

    if verbose > 0 {
        anstream::eprintln!("bridging to wss://{hostname}:{port}/");
    }

    let connector = Some(tls_connector(insecure_tls)?);
    let (socket, _) = connect_async_tls_with_config(request, None, false, connector)
        .await
        .with_context(|| format!("connecting to wss://{hostname}:{port}/"))?;

    let (mut sink, mut stream) = socket.split();

    let uplink = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buffer = vec![0u8; READ_BUFFER];
        loop {
            let read = match stdin.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            if sink
                .send(Message::binary(buffer[..read].to_vec()))
                .await
                .is_err()
            {
                break;
            }
        }
        let _ = sink.send(Message::Close(None)).await;
    });

    let mut stdout = tokio::io::stdout();
    while let Some(message) = stream.next().await {
        let payload = match message.context("reading from the job service")? {
            Message::Binary(payload) => payload,
            Message::Text(text) => text.into(),
            Message::Close(_) => break,
            _ => continue,
        };
        if stdout.write_all(&payload).await.is_err() || stdout.flush().await.is_err() {
            break;
        }
    }

    uplink.abort();
    Ok(())
}

/// TLS configuration.
///
/// This pins `http/1.1`. WebSockets over HTTP/2 needs extended CONNECT of RFC
/// 8441. This would cause a gateway that negotiates h2 to fail the upgrade
/// silently.
fn tls_connector(insecure: bool) -> Result<Connector> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .context("building a TLS configuration")?;

    let mut config = if insecure {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate(provider)))
            .with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        let native = rustls_native_certs::load_native_certs();
        let (added, _) = roots.add_parsable_certificates(native.certs);
        if added == 0 {
            bail!("found no usable system TLS root certificates to verify the gateway against");
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    };

    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Connector::Rustls(Arc::new(config)))
}

#[derive(Debug)]
struct AcceptAnyCertificate(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

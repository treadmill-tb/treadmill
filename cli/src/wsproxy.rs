use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
};
use uuid::Uuid;

use crate::ctx::Ctx;
use crate::ssh::{select_endpoint, service_credentials};
use crate::state::State;

const READ_BUFFER: usize = 16 * 1024;

/// Bridge stdin/stdout to a job's `sshws` service: the gateway and the job's
/// own proxy both admit the request against the service token, and the socket
/// then carries raw SSH bytes in binary frames.
pub async fn run(ctx: &mut Ctx, job_id: Uuid, service: &str, gateway: Option<&str>) -> Result<()> {
    let ssh_domains = match gateway {
        Some(_) => ctx.config.ssh_domains(&ctx.profile)?.to_vec(),
        None => Vec::new(),
    };
    let credentials = service_credentials(ctx, job_id, service).await?;
    let endpoint = select_endpoint(&credentials, gateway, &ssh_domains)?;
    let socket = match open(endpoint, &credentials.token, ctx.insecure_tls, ctx.verbose).await {
        Ok(socket) => socket,
        Err(first_error) => {
            State::invalidate_job_service_token(
                &ctx.state_path,
                job_id,
                service,
                &credentials.token,
            )?;
            ctx.state = State::load(&ctx.state_path)?;
            let replacement = service_credentials(ctx, job_id, service).await?;
            let endpoint = select_endpoint(&replacement, gateway, &ssh_domains)?;
            open(endpoint, &replacement.token, ctx.insecure_tls, ctx.verbose)
                .await
                .with_context(|| {
                    format!("retrying after the first connection failed: {first_error:#}")
                })?
        }
    };

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

async fn open(
    endpoint: &treadmill_rs::api::switchboard::jobs::JobServiceEndpoint,
    token: &str,
    insecure_tls: bool,
    verbose: u8,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
    let hostname = &endpoint.hostname;
    let port = endpoint.port;
    let mut request = format!("wss://{hostname}:{port}/")
        .into_client_request()
        .context("building the WebSocket request")?;
    request.headers_mut().insert(
        "X-Tml-Token",
        HeaderValue::from_str(token).context("the service token is not a valid header value")?,
    );

    if verbose > 0 {
        anstream::eprintln!("bridging to wss://{hostname}:{port}/");
        if let Some(claims) = describe_claims(token) {
            anstream::eprintln!("{claims}");
        }
    }

    let connector = Some(tls_connector(insecure_tls)?);
    connect_async_tls_with_config(request, None, false, connector)
        .await
        .map(|(socket, _)| socket)
        .map_err(|error| handshake_error(error, hostname, port))
}

/// Decodes JWT claims without checking its signature, used for debugging.
///
/// We don't want to leak the actual token to the CLI. This technically breaks
/// the rule to not try and interpret the switchboard-returned JWT, but it's a
/// fairly rigid contract between the switchboard and hosts anyways, and in the
/// worst case this function just fails.
fn describe_claims(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let field = |name: &str| {
        claims
            .get(name)
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string()
    };
    let expires_at = claims
        .get("exp")
        .and_then(|v| v.as_i64())
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| "?".to_string());

    Some(format!(
        "token claims: aud={} tml_job={} tml_service={} sub={} expires_at={expires_at}",
        field("aud"),
        field("tml_job"),
        field("tml_service"),
        field("sub"),
    ))
}

/// Surfaces the HTTP response body from a rejected handshake, for debugging.
fn handshake_error(error: WsError, hostname: &str, port: u16) -> anyhow::Error {
    let WsError::Http(response) = &error else {
        return anyhow::Error::new(error)
            .context(format!("connecting to wss://{hostname}:{port}/"));
    };

    let status = response.status();
    let body = response
        .body()
        .as_deref()
        .map(String::from_utf8_lossy)
        .map(|body| body.trim().to_string())
        .filter(|body| !body.is_empty());

    match body {
        Some(body) => anyhow::anyhow!(
            "connecting to wss://{hostname}:{port}/: HTTP error: {status}, \
             {body:?}; re-run with `-v` to see JWT claims"
        ),
        None => anyhow::anyhow!(
            "connecting to wss://{hostname}:{port}/: HTTP error: {status} \
             (no response body); re-run with `-v` to see JWT claims"
        ),
    }
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

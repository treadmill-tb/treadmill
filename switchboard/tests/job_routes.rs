//! Route tests for `POST /jobs/{id}/log-token` (NATS read-token minting).
//!
//! Drives the real router over a loopback socket against ephemeral Postgres,
//! using the development mock-OAuth provider to obtain an authenticated caller
//! (no external service). `alice` is provisioned as a global admin (so she can
//! read any job — no job row needs to exist), `bob` is a plain user. No live
//! NATS is involved: minting the token is pure (the wire round-trip lives in
//! the `nats-log-streaming` Nix check).
//!
//! Queries here use sqlx's runtime API (not the `query!` macros), so the test
//! needs no entry in the offline `.sqlx` cache.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::redirect::Policy;
use sqlx::PgPool;
use tokio::net::TcpListener;
use uuid::Uuid;

use treadmill_rs::api::switchboard::LoginResponse;
use treadmill_rs::api::switchboard::jobs::LogStreamCredentials;
use treadmill_switchboard::config::LogStreamingConfig;
use treadmill_switchboard::log_streaming::{LogStreamProvisioner, LogStreaming, ProvisionError};
use treadmill_switchboard::registry::OciRegistryClient;
use treadmill_switchboard::routes::build_router;
use treadmill_switchboard::serve::AppState;

mod common;
use common::test_config_mock;

/// A provisioner the read route never calls (it only mints tokens); present
/// only to satisfy the `LogStreaming` struct.
struct NoopProvisioner;

#[async_trait::async_trait]
impl LogStreamProvisioner for NoopProvisioner {
    async fn ensure_job_stream(&self, _job_id: Uuid) -> Result<(), ProvisionError> {
        Ok(())
    }
}

/// A streaming-enabled `LogStreaming` with a throwaway account seed (generated
/// per run, so no real secret enters the source tree).
fn test_log_streaming() -> LogStreaming {
    let account = nats_jwt::KeyPair::new_account();
    LogStreaming {
        config: LogStreamingConfig {
            nats_url: "nats://nats.example:4222".to_string(),
            jetstream_domain: None,
            account_seed: account.seed().expect("account seed"),
        },
        provisioner: Arc::new(NoopProvisioner),
    }
}

fn streaming_enabled_state(pool: PgPool) -> AppState {
    AppState::with_components(
        pool,
        test_config_mock(),
        Arc::new(OciRegistryClient::new()),
        Some(test_log_streaming()),
    )
}

async fn spawn_server(state: AppState) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(state);
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    addr
}

/// Drive a full mock login for `identity` and return the issued bearer token
/// (HTTP-encoded), ready for `bearer_auth`.
async fn mock_login_token(client: &reqwest::Client, addr: SocketAddr, identity: &str) -> String {
    let login = client
        .get(format!(
            "http://{addr}/api/v1/auth/mock/login?identity={identity}"
        ))
        .send()
        .await
        .unwrap();
    assert!(
        login.status().is_redirection(),
        "mock login should redirect"
    );
    let location = login
        .headers()
        .get(reqwest::header::LOCATION)
        .expect("redirect must carry a Location")
        .to_str()
        .unwrap()
        .to_string();

    let cb = client
        .get(format!("http://{addr}{location}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        cb.status(),
        reqwest::StatusCode::OK,
        "callback should succeed"
    );
    let session: LoginResponse = cb.json().await.unwrap();
    session.token.encode_for_http()
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn admin_gets_a_subscribe_token_for_any_job(pool: PgPool) {
    let addr = spawn_server(streaming_enabled_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    // `alice` is a global admin, so she can read any job — including one that
    // does not exist as a row, which is fine: the token is job-scoped by id.
    let token = mock_login_token(&client, addr, "alice").await;
    let job_id = Uuid::new_v4();

    let resp = client
        .post(format!("http://{addr}/api/v1/jobs/{job_id}/log-token"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let creds: LogStreamCredentials = resp.json().await.unwrap();
    assert_eq!(creds.nats_url, "nats://nats.example:4222");
    assert_eq!(creds.subject, format!("logs.{job_id}.>"));
    assert_eq!(creds.expires_in_secs, 300);
    assert!(!creds.token.is_empty(), "a token must be issued");
    // Sanity: a JWT has three dot-separated segments. (The token's scope/shape
    // is asserted exhaustively in the log_streaming unit tests.)
    assert_eq!(creds.token.split('.').count(), 3, "token looks like a JWT");
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn non_reader_is_forbidden(pool: PgPool) {
    let addr = spawn_server(streaming_enabled_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    // `bob` is a plain user with no grant on (and no ownership of) the job.
    let token = mock_login_token(&client, addr, "bob").await;
    let job_id = Uuid::new_v4();

    let resp = client
        .post(format!("http://{addr}/api/v1/jobs/{job_id}/log-token"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn streaming_disabled_yields_service_unavailable(pool: PgPool) {
    // `AppState::new` leaves log streaming unconfigured (None).
    let addr = spawn_server(AppState::new(pool.clone(), test_config_mock())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    // Use the admin so authorization passes and we reach the streaming check.
    let token = mock_login_token(&client, addr, "alice").await;
    let job_id = Uuid::new_v4();

    let resp = client
        .post(format!("http://{addr}/api/v1/jobs/{job_id}/log-token"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
}

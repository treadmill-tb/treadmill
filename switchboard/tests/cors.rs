//! Tests for the API's configurable CORS allowlist
//! (`server.cors_allowed_origins`).
//!
//! Drives the real router over loopback and asserts on the response headers:
//! a preflight `OPTIONS` answered by the layer itself, actual requests from
//! allowed and non-allowed origins, the `"*"` opt-in, and the default (empty)
//! configuration emitting no CORS headers at all.

use std::net::SocketAddr;

use sqlx::PgPool;
use tokio::net::TcpListener;
use treadmill_switchboard::routes::build_router;
use treadmill_switchboard::serve::AppState;

mod common;
use common::test_config_mock;

const CONSOLE_ORIGIN: &str = "https://console.example";

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

async fn spawn_with_origins(pool: PgPool, origins: &[&str]) -> SocketAddr {
    let mut cfg = test_config_mock();
    cfg.server.cors_allowed_origins = origins.iter().map(|o| o.to_string()).collect();
    spawn_server(AppState::new(pool, cfg)).await
}

fn header<'r>(resp: &'r reqwest::Response, name: &str) -> Option<&'r str> {
    resp.headers().get(name).map(|v| v.to_str().unwrap())
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn allowlisted_origin_is_granted(pool: PgPool) {
    let addr = spawn_with_origins(pool, &[CONSOLE_ORIGIN]).await;
    let client = reqwest::Client::new();

    // Preflight: answered by the CORS layer itself, no auth involved.
    let preflight = client
        .request(
            reqwest::Method::OPTIONS,
            format!("http://{addr}/api/v1/jobs"),
        )
        .header("Origin", CONSOLE_ORIGIN)
        .header("Access-Control-Request-Method", "POST")
        .header("Access-Control-Request-Headers", "authorization")
        .send()
        .await
        .unwrap();
    assert!(preflight.status().is_success());
    assert_eq!(
        header(&preflight, "access-control-allow-origin"),
        Some(CONSOLE_ORIGIN)
    );
    let methods = header(&preflight, "access-control-allow-methods").unwrap();
    for m in ["GET", "POST", "PATCH", "PUT", "DELETE"] {
        assert!(methods.contains(m), "{m} missing from {methods}");
    }
    let headers = header(&preflight, "access-control-allow-headers").unwrap();
    assert!(headers.to_ascii_lowercase().contains("authorization"));

    // An actual (unauthenticated) request carries the grant too.
    let resp = client
        .get(format!("http://{addr}/api/v1/auth/providers"))
        .header("Origin", CONSOLE_ORIGIN)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    assert_eq!(
        header(&resp, "access-control-allow-origin"),
        Some(CONSOLE_ORIGIN)
    );

    // Even an error response (401 here) is CORS-visible, so the SPA can read
    // the status rather than seeing an opaque network failure.
    let unauth = client
        .get(format!("http://{addr}/api/v1/jobs"))
        .header("Origin", CONSOLE_ORIGIN)
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(
        header(&unauth, "access-control-allow-origin"),
        Some(CONSOLE_ORIGIN)
    );
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn unlisted_origin_gets_no_grant(pool: PgPool) {
    let addr = spawn_with_origins(pool, &[CONSOLE_ORIGIN]).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{addr}/api/v1/auth/providers"))
        .header("Origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    assert_eq!(header(&resp, "access-control-allow-origin"), None);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn wildcard_grants_any_origin(pool: PgPool) {
    let addr = spawn_with_origins(pool, &["*"]).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{addr}/api/v1/auth/providers"))
        .header("Origin", "https://anywhere.example")
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    assert_eq!(header(&resp, "access-control-allow-origin"), Some("*"));
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn default_config_emits_no_cors_headers(pool: PgPool) {
    let addr = spawn_with_origins(pool, &[]).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{addr}/api/v1/auth/providers"))
        .header("Origin", CONSOLE_ORIGIN)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    assert_eq!(header(&resp, "access-control-allow-origin"), None);

    // Without the layer a preflight falls through to routing (no OPTIONS
    // route), so it must not succeed with CORS grants.
    let preflight = client
        .request(
            reqwest::Method::OPTIONS,
            format!("http://{addr}/api/v1/jobs"),
        )
        .header("Origin", CONSOLE_ORIGIN)
        .header("Access-Control-Request-Method", "POST")
        .send()
        .await
        .unwrap();
    assert_eq!(
        header(&preflight, "access-control-allow-origin"),
        None,
        "no grant may appear without configuration"
    );
}

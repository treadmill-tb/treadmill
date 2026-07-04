//! Verifies the optional embedded web console: when `[console] enabled`, the
//! switchboard serves the console at `/` on the same listener as the API at
//! `/api/v1`, and the console renders by calling the API back over loopback.

use std::net::SocketAddr;

use reqwest::redirect::Policy;
use sqlx::PgPool;
use tokio::net::TcpListener;
use treadmill_switchboard::config::EmbeddedConsoleConfig;
use treadmill_switchboard::routes::build_router;
use treadmill_switchboard::serve::AppState;

mod common;
use common::test_config;

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn embedded_console_is_served_next_to_the_api(pool: PgPool) {
    // Bind first so the console's default loopback URL can target the real port.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let mut config = test_config("http://unused.invalid");
    config.server.bind_address = addr;
    config.console = Some(EmbeddedConsoleConfig {
        enabled: true,
        public_base_url: None, // default loopback
        api_base_url: None,    // default loopback -> this same server
    });

    let router = build_router(AppState::new(pool, config));
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    // The console sign-in page renders at `/`-rooted paths. It is produced by
    // the console fetching /auth/providers from this same switchboard over
    // loopback, so a 200 here exercises the whole embedded round-trip.
    let login = client
        .get(format!("http://{addr}/login"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        login.status(),
        reqwest::StatusCode::OK,
        "console /login served"
    );
    let body = login.text().await.unwrap();
    assert!(body.contains("Sign in"), "renders the sign-in page");
    assert!(
        body.contains("GitHub"),
        "advertises the configured GitHub provider"
    );

    // The API still lives at /api/v1 on the same port.
    let providers = client
        .get(format!("http://{addr}/api/v1/auth/providers"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        providers.status(),
        reqwest::StatusCode::OK,
        "API still served"
    );

    // A top-level path matched by neither the console nor the API falls through
    // to the switchboard's own fallback, confirming it survives the merge (the
    // console contributes only routes, not a competing fallback).
    let missing = client
        .get(format!("http://{addr}/does-not-exist"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
    assert_eq!(missing.text().await.unwrap(), "no such route");
}

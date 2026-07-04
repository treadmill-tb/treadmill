//! End-to-end test for the development-only mock OAuth provider.
//!
//! Unlike the GitHub flow, the mock provider talks to no external service: the
//! login route self-redirects to the callback with the chosen identity as the
//! `code`. So this test needs no wiremock — just a real ephemeral Postgres (via
//! `#[sqlx::test]`) and the real router. It exercises the full path: CSRF flow,
//! callback, provisioning, the staged-login claim that mints the token, and
//! the admin grant for `alice`.
//!
//! Queries here use sqlx's runtime API (not the `query!` macros) so the test
//! needs no entry in the offline `.sqlx` cache.

use std::net::SocketAddr;

use reqwest::redirect::Policy;
use sqlx::PgPool;
use tokio::net::TcpListener;
use treadmill_rs::api::switchboard::{LoginResponse, LoginStagedResponse, WhoAmIResponse};
use treadmill_switchboard::routes::build_router;
use treadmill_switchboard::serve::AppState;
use uuid::Uuid;

mod common;
use common::test_config_mock;

/// The seeded global admins group's well-known id (matches `ADMINS_GROUP_ID`).
const ADMINS_GROUP_ID: Uuid = Uuid::from_u128(1);

/// Spawn the switchboard router on an ephemeral port; returns its address.
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

/// Drive a full mock login for `identity`: start the flow (which 302s to the
/// callback), follow that redirect, claim the staged login the callback hands
/// back, and return the issued session plus the caller's identity as reported
/// by `/auth/whoami`.
async fn run_mock_login(
    client: &reqwest::Client,
    addr: SocketAddr,
    identity: &str,
) -> (LoginResponse, WhoAmIResponse) {
    let login = client
        .get(format!(
            "http://{addr}/api/v1/auth/mock/login?identity={identity}"
        ))
        .send()
        .await
        .unwrap();
    assert!(
        login.status().is_redirection(),
        "mock login should redirect to the callback, got {}",
        login.status()
    );

    // The login route self-redirects to the callback; the relative Location is
    // the entire dance (no external service).
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
        "callback should stage the login"
    );
    let staged: LoginStagedResponse = cb.json().await.unwrap();
    assert!(
        staged.required.is_empty(),
        "mock logins are exempt from completion steps, got {:?}",
        staged.required
    );

    // Claim the staged login: the completion endpoint is what mints the token.
    let complete = client
        .post(format!("http://{addr}/api/v1/auth/login/complete"))
        .json(&serde_json::json!({
            "staged_id": staged.staged_id,
            "staged_secret": staged.staged_secret,
            "tos_version": staged.tos_version,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        complete.status(),
        reqwest::StatusCode::OK,
        "claiming the staged login should succeed"
    );
    let session: LoginResponse = complete.json().await.unwrap();

    let who: WhoAmIResponse = client
        .get(format!("http://{addr}/api/v1/auth/whoami"))
        .bearer_auth(session.token.encode_for_http())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    (session, who)
}

/// Whether `user_id` is a member of the global admins group.
async fn is_global_admin(pool: &PgPool, user_id: Uuid) -> bool {
    sqlx::query_scalar(
        "select exists(\
           select 1 from tml_switchboard.group_members \
           where group_id = $1 and member_id = $2\
         )",
    )
    .bind(ADMINS_GROUP_ID)
    .bind(user_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn mock_login_provisions_alice_as_admin(pool: PgPool) {
    let state = AppState::new(pool.clone(), test_config_mock());
    let addr = spawn_server(state).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    let (_session, who) = run_mock_login(&client, addr, "alice").await;
    assert_eq!(who.username, "alice");
    assert!(
        is_global_admin(&pool, who.user_id).await,
        "alice should be granted global admin"
    );

    // A second login is idempotent: same user, still exactly one admins
    // membership (no duplicate, no error).
    let (_again, who2) = run_mock_login(&client, addr, "alice").await;
    assert_eq!(who2.user_id, who.user_id, "re-login resolves the same user");
    let memberships: i64 = sqlx::query_scalar(
        "select count(*) from tml_switchboard.group_members \
         where group_id = $1 and member_id = $2",
    )
    .bind(ADMINS_GROUP_ID)
    .bind(who.user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(memberships, 1, "admin membership must not be duplicated");
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn mock_login_bob_is_not_admin(pool: PgPool) {
    let state = AppState::new(pool.clone(), test_config_mock());
    let addr = spawn_server(state).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    let (_session, who) = run_mock_login(&client, addr, "bob").await;
    assert_eq!(who.username, "bob");
    assert!(
        !is_global_admin(&pool, who.user_id).await,
        "bob is a plain user and must not be an admin"
    );
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn mock_login_rejects_unknown_identity(pool: PgPool) {
    let state = AppState::new(pool.clone(), test_config_mock());
    let addr = spawn_server(state).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    let resp = client
        .get(format!(
            "http://{addr}/api/v1/auth/mock/login?identity=nobody"
        ))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_client_error() || resp.status().is_server_error(),
        "an unknown mock identity must be rejected, got {}",
        resp.status()
    );

    // The real invariant: no CSRF flow was persisted for the bogus request.
    let flows: i64 = sqlx::query_scalar("select count(*) from tml_switchboard.oauth_flows")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(flows, 0, "a rejected login must not start a flow");
}

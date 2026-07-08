//! Shared helpers for the switchboard integration tests.
//!
//! Each file under `tests/` is compiled as its own crate, so code shared
//! between them lives here and is pulled in with `mod common;`. Placing it at
//! `common/mod.rs` (rather than `common.rs`) keeps cargo from treating it as a
//! standalone test binary.

// Each test crate uses only the subset of helpers it needs; the rest look dead
// from that crate's point of view.
#![allow(dead_code)]

use std::net::SocketAddr;

use sqlx::PgPool;
use tokio::net::TcpListener;
use uuid::Uuid;

use treadmill_rs::api::switchboard::{LoginResponse, LoginStagedResponse, WhoAmIResponse};
use treadmill_switchboard::auth::engine::ADMINS_GROUP_ID;
use treadmill_switchboard::config::{
    DatabaseConfig, GitHubOAuthConfig, MockOAuthConfig, OAuthConfig, ServerConfig, ServiceConfig,
    SwitchboardConfig,
};
use treadmill_switchboard::routes::build_router;
use treadmill_switchboard::serve::AppState;

/// A throwaway [`SwitchboardConfig`] for tests.
///
/// `gh_uri` points the GitHub token/API endpoints at a local mock (wiremock);
/// the database and the rest of the config are inert placeholders. The auth URL
/// is never contacted — tests drive the callback directly rather than visiting
/// the consent screen.
pub fn test_config(gh_uri: &str) -> SwitchboardConfig {
    SwitchboardConfig {
        database: DatabaseConfig {
            host: "unused".to_string(),
            port: None,
            database: "unused".to_string(),
            user: "unused".to_string(),
            auth: None,
        },
        server: ServerConfig {
            bind_address: "127.0.0.1:0".parse().unwrap(),
            trusted_proxy_headers: Vec::new(),
            cors_allowed_origins: Vec::new(),
        },
        service: ServiceConfig {
            current_tos_version: 1,
            default_token_timeout: chrono::Duration::hours(1),
            default_job_timeout: chrono::Duration::hours(1),
            default_queue_timeout: chrono::Duration::hours(1),
            match_interval: chrono::Duration::seconds(1),
            host_liveness_timeout: chrono::Duration::seconds(30),
            supervisor_ping_interval: std::time::Duration::from_secs(30),
            supervisor_pong_dead: std::time::Duration::from_secs(60),
            supervisor_reconcile_interval: std::time::Duration::from_secs(30),
        },
        oauth: OAuthConfig {
            github: Some(GitHubOAuthConfig {
                client_id: "test-client".to_string(),
                client_secret: "test-secret".to_string(),
                redirect_url: "http://localhost/api/v1/auth/github/callback".to_string(),
                auth_url: "http://localhost/login/oauth/authorize".to_string(),
                token_url: format!("{gh_uri}/login/oauth/access_token"),
                api_base_url: gh_uri.to_string(),
            }),
            mock: None,
            return_to_allowlist: Vec::new(),
        },
        log_streaming: None,
    }
}

/// A [`SwitchboardConfig`] with only the development-only mock provider enabled
/// (no GitHub). Used by the mock-login test, which needs no external server.
pub fn test_config_mock() -> SwitchboardConfig {
    let mut cfg = test_config("http://unused.invalid");
    cfg.oauth.github = None;
    cfg.oauth.mock = Some(MockOAuthConfig { enabled: true });
    cfg
}

/// Spawn the switchboard router on an ephemeral port; returns its address.
pub async fn spawn_server(state: AppState) -> SocketAddr {
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
/// back (first login accepts ToS), and return the issued session plus the
/// caller's identity as reported by `/auth/whoami`. The `client` must not
/// follow redirects.
///
/// Login never confers admin; tests that need an admin caller grant the
/// membership explicitly via [`grant_admin`].
pub async fn run_mock_login(
    pool: &PgPool,
    client: &reqwest::Client,
    addr: SocketAddr,
    identity: &str,
    first_login: bool, // whether a ToS accept is expected
) -> (LoginResponse, WhoAmIResponse) {
    // Provision all three users as allowed users to login, idempotent:
    sqlx::query(
        "insert into tml_switchboard.login_allowlist \
         (provider, kind, external_id) \
         values \
         ('mock', 'user', 'alice'), \
         ('mock', 'user', 'bob'), \
         ('mock', 'user', 'carol') \
         on conflict do nothing
         ",
    )
    .execute(pool)
    .await
    .unwrap();

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
    assert_eq!(
        staged.required,
        if first_login { &["tos"][..] } else { &[][..] },
        "only first login should be subject to ToS accept",
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

/// Make `user_id` a global admin the way a deployment would: an explicit,
/// external membership insert. Idempotent.
pub async fn grant_admin(pool: &PgPool, user_id: Uuid) {
    sqlx::query(
        "insert into tml_switchboard.group_members \
         (group_id, member_id, source, source_ref) \
         values ($1, $2, 'manual', '') \
         on conflict do nothing",
    )
    .bind(ADMINS_GROUP_ID)
    .bind(user_id)
    .execute(pool)
    .await
    .unwrap();
}

/// [`run_mock_login`] reduced to the issued bearer token, for route tests that
/// only need an authenticated caller. `alice` is additionally provisioned as a
/// global admin (the external grant a deployment would perform), so tests can
/// use her as the admin caller.
pub async fn mock_login_token(
    pool: &PgPool,
    client: &reqwest::Client,
    addr: SocketAddr,
    identity: &str,
    first_login: bool,
) -> String {
    let (session, who) = run_mock_login(pool, client, addr, identity, first_login).await;
    if identity == "alice" {
        grant_admin(pool, who.user_id).await;
    }
    session.token.encode_for_http()
}

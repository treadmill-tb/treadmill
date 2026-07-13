//! End-to-end tests for the user-management REST API.
//!
//! Each test provisions a real user by driving the GitHub OAuth callback against
//! a `wiremock` stand-in (same approach as `oauth_login.rs`), then exercises the
//! `/users` routes over HTTP against the real router and an ephemeral Postgres.

use std::net::SocketAddr;

use sqlx::PgPool;
use tokio::net::TcpListener;
use treadmill_rs::api::switchboard::LoginResponse;
use treadmill_rs::api::switchboard::users::{
    PublicUserProfile, SelfUserProfile, SessionInfo, UserEmail,
};
use treadmill_switchboard::routes::build_router;
use treadmill_switchboard::serve::AppState;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;
use common::test_config;

/// Mount canned GitHub responses for the canonical "octocat" identity.
async fn mount_github(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "gho_test_token",
            "token_type": "bearer",
            "scope": "read:user,user:email,read:org",
        })))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 12345,
            "login": "octocat",
            "name": "The Octocat",
            "avatar_url": "https://example.com/octocat.png",
        })))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path("/user/emails"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "email": "octo@example.com", "verified": true, "primary": true },
        ])))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path("/user/memberships/orgs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(server)
        .await;
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

/// Drive one full login and return the issued session.
async fn run_login(client: &reqwest::Client, addr: SocketAddr, pool: &PgPool) -> LoginResponse {
    let login_resp = client
        .get(format!("http://{addr}/api/v1/auth/github/login"))
        .send()
        .await
        .unwrap();
    assert!(login_resp.status().is_redirection());

    let state: String = sqlx::query_scalar("select state from tml_switchboard.oauth_flows")
        .fetch_one(pool)
        .await
        .unwrap();

    let cb_resp = client
        .get(format!(
            "http://{addr}/api/v1/auth/github/callback?code=CANNED_CODE&state={state}"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        cb_resp.status(),
        reqwest::StatusCode::OK,
        "callback should stage the login"
    );
    let body: serde_json::Value = cb_resp.json().await.unwrap();

    // Claim the staged login, echoing the offered ToS version (null when no
    // consent is required).
    let complete = client
        .post(format!("http://{addr}/api/v1/auth/login/complete"))
        .json(&serde_json::json!({
            "staged_id": body["staged_id"],
            "staged_secret": body["staged_secret"],
            "tos_version": body["tos_version"],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(complete.status(), reqwest::StatusCode::OK);
    complete.json().await.unwrap()
}

/// Provision a user via login and return (server addr, bearer token, user_id).
async fn login_user(pool: &PgPool) -> (SocketAddr, String, Uuid, MockServer) {
    let gh = MockServer::start().await;
    mount_github(&gh).await;
    // The admission gate now guards new-user registration; allow-list the canned
    // "octocat" identity so this helper can still provision it via login.
    sqlx::query(
        "insert into tml_switchboard.login_allowlist (provider, kind, external_id, comment) \
         values ('github', 'user', '12345', 'test fixture')",
    )
    .execute(pool)
    .await
    .unwrap();
    let addr = spawn_server(AppState::new(pool.clone(), test_config(&gh.uri()))).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let session = run_login(&client, addr, pool).await;
    let token = session.token.encode_for_http();
    let user_id: Uuid = sqlx::query_scalar(
        "select user_id from tml_switchboard.user_identities \
         where provider = 'github' and provider_user_id = '12345'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    // Keep the mock server alive for the duration of the test by returning it.
    (addr, token, user_id, gh)
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn self_profile_update_tokens_and_feed(pool: PgPool) {
    let (addr, token, user_id, _gh) = login_user(&pool).await;
    let client = reqwest::Client::new();
    let base = format!("http://{addr}/api/v1");

    // GET /users/me folds in emails, groups, lock state, and the linked GitHub.
    let me: SelfUserProfile = client
        .get(format!("{base}/users/me"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me.profile.user_id, user_id);
    assert_eq!(me.profile.name, "The Octocat");
    assert_eq!(
        me.emails,
        vec![UserEmail {
            email: "octo@example.com".to_string(),
            verified: true,
            is_primary: true,
        }],
    );
    assert!(!me.locked);
    let gh = me.profile.github.expect("linked github identity");
    assert_eq!(gh.login, "octocat");
    assert_eq!(gh.profile_url, "https://github.com/octocat");

    // PATCH /users/me with a new display name + avatar succeeds and is
    // reflected. The name is stored trimmed.
    let resp = client
        .patch(format!("{base}/users/me"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "name": "  Octo Cat  ",
            "avatar_url": "https://example.com/new.png",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let updated: SelfUserProfile = resp.json().await.unwrap();
    assert_eq!(updated.profile.name, "Octo Cat");
    assert_eq!(
        updated.profile.avatar_url.as_deref(),
        Some("https://example.com/new.png")
    );

    // A blank name, a name that is only whitespace, one carrying control
    // characters, and an over-long one are each rejected with 400, not 500.
    let too_long = "x".repeat(257);
    for bad in ["", "   ", "bad\u{7}name", too_long.as_str()] {
        let resp = client
            .patch(format!("{base}/users/me"))
            .bearer_auth(&token)
            .json(&serde_json::json!({ "name": bad }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::BAD_REQUEST,
            "name {bad:?} should be rejected"
        );
    }

    // A malformed avatar URL is rejected.
    let resp = client
        .patch(format!("{base}/users/me"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "avatar_url": "not-a-url" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

    // GET /users/{id} returns only the public subset (no emails/groups/locked).
    let other_id = Uuid::new_v4();
    sqlx::query("insert into tml_switchboard.subjects (subject_id, kind) values ($1, 'user')")
        .bind(other_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("insert into tml_switchboard.users (subject_id, name) values ($1, 'Other User')")
        .bind(other_id)
        .execute(&pool)
        .await
        .unwrap();
    let public_raw: serde_json::Value = client
        .get(format!("{base}/users/{other_id}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(public_raw["name"], "Other User");
    assert!(public_raw.get("emails").is_none());
    assert!(public_raw.get("groups").is_none());
    assert!(public_raw.get("locked").is_none());
    // And it deserializes as the public type.
    let _public: PublicUserProfile = serde_json::from_value(public_raw).unwrap();

    // The self audit feed surfaces the login events.
    let feed: serde_json::Value = client
        .get(format!("{base}/users/{user_id}/events"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let events = feed["events"].as_array().expect("events array");
    assert!(
        events
            .iter()
            .any(|e| e["event_type"] == "user_logged_in.v1"),
        "self feed should include the login event, got {events:?}"
    );

    // GET /users/me/tokens lists the session and flags the current one.
    let tokens: Vec<SessionInfo> = client
        .get(format!("{base}/users/me/tokens"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let current = tokens
        .iter()
        .find(|t| t.current)
        .expect("the request token is flagged current");
    assert!(current.revoked.is_none());
    let token_id = current.token_id;

    // DELETE revokes it; the now-revoked token stops authenticating.
    let resp = client
        .delete(format!("{base}/users/me/tokens/{token_id}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);

    let resp = client
        .get(format!("{base}/users/me"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn revoking_another_users_token_is_not_found(pool: PgPool) {
    let (addr, token, _user_id, _gh) = login_user(&pool).await;
    let client = reqwest::Client::new();
    let base = format!("http://{addr}/api/v1");

    // A token id that is not the caller's (here, simply random) is reported as
    // not-found rather than revealing whether it exists.
    let stranger_token = Uuid::new_v4();
    let resp = client
        .delete(format!("{base}/users/me/tokens/{stranger_token}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn locked_account_is_rejected(pool: PgPool) {
    let (addr, token, user_id, _gh) = login_user(&pool).await;
    let client = reqwest::Client::new();
    let base = format!("http://{addr}/api/v1");

    // The token works before the account is locked.
    let resp = client
        .get(format!("{base}/users/me"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    sqlx::query("update tml_switchboard.users set locked = true where subject_id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();

    // Once locked, the existing token is refused at extraction time with 403.
    let resp = client
        .get(format!("{base}/users/me"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

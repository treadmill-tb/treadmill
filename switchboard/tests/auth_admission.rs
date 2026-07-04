//! End-to-end tests for the OAuth admission gate.
//!
//! Drives the real GitHub authorization-code flow against a `wiremock` stand-in
//! (same approach as `oauth_login.rs`) and asserts the gate's behavior on the
//! new-user registration path: an identity that is neither individually
//! allow-listed nor a member of an allow-listed org is denied with NO user
//! record and an operator-only `registration_denied` audit event; an
//! allow-listed identity (or a member of an allow-listed org) is provisioned;
//! and an existing user is unaffected by a later allow-list removal.
//!
//! Queries here use sqlx's runtime API (not the `query!` macros) so the test
//! needs no entry in the offline `.sqlx` cache.

use std::net::SocketAddr;

use sqlx::PgPool;
use tokio::net::TcpListener;
use treadmill_rs::api::switchboard::LoginResponse;
use treadmill_switchboard::routes::build_router;
use treadmill_switchboard::serve::AppState;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;
use common::test_config;

/// Mount canned GitHub responses for the "octocat" identity (numeric id 12345),
/// reporting active membership in each org in `org_ids`.
async fn mount_github(server: &MockServer, org_ids: &[i64]) {
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

    let orgs: Vec<_> = org_ids
        .iter()
        .map(|id| serde_json::json!({ "state": "active", "organization": { "id": id } }))
        .collect();
    Mock::given(method("GET"))
        .and(path("/user/memberships/orgs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!(orgs)))
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

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// Start a login flow (persisting the CSRF state), read that state back out of
/// the database, then drive the callback and return its raw HTTP status.
async fn drive_callback(
    client: &reqwest::Client,
    addr: SocketAddr,
    pool: &PgPool,
) -> reqwest::StatusCode {
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

    client
        .get(format!(
            "http://{addr}/api/v1/auth/github/callback?code=CANNED_CODE&state={state}"
        ))
        .send()
        .await
        .unwrap()
        .status()
}

/// Drive a full interactive login, transparently completing the ToS
/// interstitial when the callback returns it. Returns the final HTTP status: the
/// callback's status when it did not require ToS (e.g. a `403` denial, or an
/// existing user with a current ToS), otherwise the `/auth/tos/accept` status.
async fn drive_full_login(
    client: &reqwest::Client,
    addr: SocketAddr,
    pool: &PgPool,
) -> reqwest::StatusCode {
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

    let cb = client
        .get(format!(
            "http://{addr}/api/v1/auth/github/callback?code=CANNED_CODE&state={state}"
        ))
        .send()
        .await
        .unwrap();
    if cb.status() != reqwest::StatusCode::CONFLICT {
        return cb.status();
    }

    let body: serde_json::Value = cb.json().await.unwrap();
    let pending_id = body["pending_id"].as_str().unwrap().to_string();
    client
        .post(format!("http://{addr}/api/v1/auth/tos/accept"))
        .json(&serde_json::json!({ "pending_id": pending_id }))
        .send()
        .await
        .unwrap()
        .status()
}

/// Count staged (not-yet-accepted) registrations.
async fn pending_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("select count(*) from tml_switchboard.pending_registrations")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The octocat user's currently-accepted ToS version, if the user exists.
async fn octocat_tos_version(pool: &PgPool) -> Option<i32> {
    sqlx::query_scalar(
        "select u.tos_accepted_version from tml_switchboard.users u \
         join tml_switchboard.user_identities i on i.user_id = u.subject_id \
         where i.provider = 'github' and i.provider_user_id = '12345'",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Count local user rows for the canned octocat identity.
async fn octocat_user_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "select count(*) from tml_switchboard.user_identities \
         where provider = 'github' and provider_user_id = '12345'",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn registration_denied_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "select count(*) from tml_switchboard.audit_events \
         where event_type = 'registration_denied.v1'",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn unlisted_identity_is_denied_without_record(pool: PgPool) {
    let gh = MockServer::start().await;
    mount_github(&gh, &[]).await;
    let addr = spawn_server(AppState::new(pool.clone(), test_config(&gh.uri()))).await;

    // No allow-list entry and no org membership -> denied with 403.
    let status = drive_callback(&client(), addr, &pool).await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);

    // No user record was created.
    assert_eq!(
        octocat_user_count(&pool).await,
        0,
        "a denied registration must leave no user_identities row"
    );
    let subjects: i64 = sqlx::query_scalar(
        "select count(*) from tml_switchboard.users where username like 'octocat%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(subjects, 0, "a denied registration must leave no users row");

    // An operator-only registration_denied audit event was recorded, attributed
    // to the anonymous subject, carrying the not_allowlisted reason.
    assert_eq!(registration_denied_count(&pool).await, 1);
    let (actor_id, payload): (uuid::Uuid, serde_json::Value) = sqlx::query_as(
        "select actor_id, payload from tml_switchboard.audit_events \
         where event_type = 'registration_denied.v1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        actor_id,
        uuid::Uuid::from_u128(3),
        "actor is the anonymous subject"
    );
    assert_eq!(payload["provider"], "github");
    assert_eq!(payload["provider_user_id"], "12345");
    assert_eq!(payload["login"], "octocat");
    assert_eq!(payload["reason"], "not_allowlisted");
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn allowlisted_user_is_provisioned(pool: PgPool) {
    let gh = MockServer::start().await;
    mount_github(&gh, &[]).await;
    let addr = spawn_server(AppState::new(pool.clone(), test_config(&gh.uri()))).await;

    // Individually allow-list the identity (kind = 'user').
    sqlx::query(
        "insert into tml_switchboard.login_allowlist (provider, kind, external_id) \
         values ('github', 'user', '12345')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Admitted, but the user must clear the ToS interstitial before any record
    // exists; `drive_full_login` accepts it and completes the login.
    let status = drive_full_login(&client(), addr, &pool).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(
        octocat_user_count(&pool).await,
        1,
        "allow-listed user provisioned"
    );
    assert_eq!(registration_denied_count(&pool).await, 0);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn org_member_is_provisioned(pool: PgPool) {
    let gh = MockServer::start().await;
    // Identity is a member of org 99, which is allow-listed by org.
    mount_github(&gh, &[99]).await;
    let addr = spawn_server(AppState::new(pool.clone(), test_config(&gh.uri()))).await;

    sqlx::query(
        "insert into tml_switchboard.login_allowlist (provider, kind, external_id) \
         values ('github', 'org', '99')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let status = drive_full_login(&client(), addr, &pool).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(octocat_user_count(&pool).await, 1, "org member provisioned");
    assert_eq!(registration_denied_count(&pool).await, 0);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn existing_user_is_unaffected_by_gate(pool: PgPool) {
    let gh = MockServer::start().await;
    mount_github(&gh, &[]).await;
    let addr = spawn_server(AppState::new(pool.clone(), test_config(&gh.uri()))).await;

    // Allow-list, register, then REMOVE the allow-list entry.
    sqlx::query(
        "insert into tml_switchboard.login_allowlist (provider, kind, external_id) \
         values ('github', 'user', '12345')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let c = client();
    assert_eq!(
        drive_full_login(&c, addr, &pool).await,
        reqwest::StatusCode::OK
    );
    assert_eq!(octocat_user_count(&pool).await, 1);

    sqlx::query("delete from tml_switchboard.login_allowlist")
        .execute(&pool)
        .await
        .unwrap();

    // The now-existing user logs in again: the gate is not consulted, and their
    // ToS is already current, so the callback logs them in directly (no
    // interstitial) and no denial is recorded.
    assert_eq!(
        drive_full_login(&c, addr, &pool).await,
        reqwest::StatusCode::OK
    );
    assert_eq!(
        octocat_user_count(&pool).await,
        1,
        "still exactly one identity"
    );
    assert_eq!(registration_denied_count(&pool).await, 0);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn new_user_staged_then_provisioned_on_tos_accept(pool: PgPool) {
    let gh = MockServer::start().await;
    mount_github(&gh, &[]).await;
    let addr = spawn_server(AppState::new(pool.clone(), test_config(&gh.uri()))).await;

    sqlx::query(
        "insert into tml_switchboard.login_allowlist (provider, kind, external_id) \
         values ('github', 'user', '12345')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let c = client();

    // Drive login + callback. Admitted, so the callback returns the 409 ToS
    // interstitial and stages a pending registration -- but creates NO user.
    let login_resp = c
        .get(format!("http://{addr}/api/v1/auth/github/login"))
        .send()
        .await
        .unwrap();
    assert!(login_resp.status().is_redirection());
    let state: String = sqlx::query_scalar("select state from tml_switchboard.oauth_flows")
        .fetch_one(&pool)
        .await
        .unwrap();
    let cb = c
        .get(format!(
            "http://{addr}/api/v1/auth/github/callback?code=CANNED_CODE&state={state}"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(cb.status(), reqwest::StatusCode::CONFLICT);
    let body: serde_json::Value = cb.json().await.unwrap();
    assert_eq!(body["tos_required"], true);
    assert_eq!(body["tos_version"], 1);
    let pending_id = body["pending_id"].as_str().unwrap().to_string();

    // Between callback and accept: no user yet, exactly one staged registration.
    assert_eq!(
        octocat_user_count(&pool).await,
        0,
        "no user record before ToS acceptance"
    );
    assert_eq!(pending_count(&pool).await, 1, "one pending registration");

    // Accept the ToS: the user is created at the current version, a token is
    // returned, and the staged row is consumed.
    let accept = c
        .post(format!("http://{addr}/api/v1/auth/tos/accept"))
        .json(&serde_json::json!({ "pending_id": pending_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(accept.status(), reqwest::StatusCode::OK);
    let _session: LoginResponse = accept.json().await.unwrap();

    assert_eq!(
        octocat_user_count(&pool).await,
        1,
        "user created on ToS acceptance"
    );
    assert_eq!(octocat_tos_version(&pool).await, Some(1));
    assert_eq!(
        pending_count(&pool).await,
        0,
        "the staged registration was consumed"
    );

    // The pending id is single-use: a second accept is 410 Gone.
    let replay = c
        .post(format!("http://{addr}/api/v1/auth/tos/accept"))
        .json(&serde_json::json!({ "pending_id": pending_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), reqwest::StatusCode::GONE);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn existing_user_reaccepts_on_tos_version_bump(pool: PgPool) {
    let gh = MockServer::start().await;
    mount_github(&gh, &[]).await;

    sqlx::query(
        "insert into tml_switchboard.login_allowlist (provider, kind, external_id) \
         values ('github', 'user', '12345')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // First login on a server whose in-force ToS is v1: register + accept.
    let addr_v1 = spawn_server(AppState::new(pool.clone(), test_config(&gh.uri()))).await;
    let c = client();
    assert_eq!(
        drive_full_login(&c, addr_v1, &pool).await,
        reqwest::StatusCode::OK
    );
    assert_eq!(octocat_user_count(&pool).await, 1);
    assert_eq!(octocat_tos_version(&pool).await, Some(1));

    // Bump the in-force ToS to v2 and stand up a fresh server on it.
    let mut cfg = test_config(&gh.uri());
    cfg.service.current_tos_version = 2;
    let addr_v2 = spawn_server(AppState::new(pool.clone(), cfg)).await;

    // The existing user logs in again: their accepted version (1) is stale, so
    // they are bounced through the interstitial with the token withheld and NO
    // duplicate record.
    let login_resp = c
        .get(format!("http://{addr_v2}/api/v1/auth/github/login"))
        .send()
        .await
        .unwrap();
    assert!(login_resp.status().is_redirection());
    let state: String = sqlx::query_scalar("select state from tml_switchboard.oauth_flows")
        .fetch_one(&pool)
        .await
        .unwrap();
    let cb = c
        .get(format!(
            "http://{addr_v2}/api/v1/auth/github/callback?code=CANNED_CODE&state={state}"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(cb.status(), reqwest::StatusCode::CONFLICT);
    let body: serde_json::Value = cb.json().await.unwrap();
    assert_eq!(body["tos_version"], 2);
    let pending_id = body["pending_id"].as_str().unwrap().to_string();

    // Still exactly one user, still recorded at v1 (no token issued yet).
    assert_eq!(
        octocat_user_count(&pool).await,
        1,
        "no duplicate record for the re-accepting user"
    );
    assert_eq!(octocat_tos_version(&pool).await, Some(1));

    // Accept v2: a token is issued, the accepted version advances, and the user
    // is still a single record.
    let accept = c
        .post(format!("http://{addr_v2}/api/v1/auth/tos/accept"))
        .json(&serde_json::json!({ "pending_id": pending_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(accept.status(), reqwest::StatusCode::OK);
    let _session: LoginResponse = accept.json().await.unwrap();

    assert_eq!(
        octocat_user_count(&pool).await,
        1,
        "still no duplicate record"
    );
    assert_eq!(octocat_tos_version(&pool).await, Some(2));
    assert_eq!(pending_count(&pool).await, 0);
}

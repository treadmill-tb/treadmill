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
/// reporting active membership in each org in `org_ids` and a single verified
/// primary email.
async fn mount_github(server: &MockServer, org_ids: &[i64]) {
    mount_github_with(
        server,
        org_ids,
        serde_json::json!([
            { "email": "octo@example.com", "verified": true, "primary": true },
        ]),
    )
    .await;
}

/// Like [`mount_github`], but with a caller-supplied `/user/emails` payload.
async fn mount_github_with(server: &MockServer, org_ids: &[i64], emails: serde_json::Value) {
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
        .respond_with(ResponseTemplate::new(200).set_body_json(emails))
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
    drive_to_callback(client, addr, pool).await.status()
}

/// Start a login flow and drive the callback, returning the raw callback
/// response (so callers can inspect the staged-login body).
async fn drive_to_callback(
    client: &reqwest::Client,
    addr: SocketAddr,
    pool: &PgPool,
) -> reqwest::Response {
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
}

/// POST the staged pair from a staged-login `body` to `/auth/login/complete`,
/// echoing the offered ToS version, and return the response.
async fn complete_login(
    client: &reqwest::Client,
    addr: SocketAddr,
    body: &serde_json::Value,
) -> reqwest::Response {
    client
        .post(format!("http://{addr}/api/v1/auth/login/complete"))
        .json(&serde_json::json!({
            "staged_id": body["staged_id"],
            "staged_secret": body["staged_secret"],
            "tos_version": body["tos_version"],
        }))
        .send()
        .await
        .unwrap()
}

/// Drive a full interactive login: the callback stages every login, so claim
/// the staged pair (echoing the offered ToS version, when consent is
/// required). Returns the final HTTP status: the callback's status when it
/// refused to stage (e.g. a `403` denial), otherwise the
/// `/auth/login/complete` status.
async fn drive_full_login(
    client: &reqwest::Client,
    addr: SocketAddr,
    pool: &PgPool,
) -> reqwest::StatusCode {
    let cb = drive_to_callback(client, addr, pool).await;
    if cb.status() != reqwest::StatusCode::OK {
        return cb.status();
    }
    let body: serde_json::Value = cb.json().await.unwrap();
    complete_login(client, addr, &body).await.status()
}

/// Count staged (not-yet-accepted) logins.
async fn staged_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("select count(*) from tml_switchboard.staged_logins")
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
        "select count(*) from tml_switchboard.users where name like 'The Octocat%'",
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
async fn allowlisted_without_verified_primary_is_denied(pool: PgPool) {
    let gh = MockServer::start().await;
    // Admitted by the allow-list, but the provider's primary address is
    // unverified: every account is seeded with a verified primary, so refuse.
    mount_github_with(
        &gh,
        &[],
        serde_json::json!([
            { "email": "octo@example.com", "verified": false, "primary": true },
            { "email": "alt@example.com", "verified": true, "primary": false },
        ]),
    )
    .await;
    sqlx::query(
        "insert into tml_switchboard.login_allowlist (provider, kind, external_id) \
         values ('github', 'user', '12345')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let addr = spawn_server(AppState::new(pool.clone(), test_config(&gh.uri()))).await;

    let status = drive_callback(&client(), addr, &pool).await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);

    // No user record, and a registration_denied event carrying the reason.
    assert_eq!(octocat_user_count(&pool).await, 0);
    assert_eq!(registration_denied_count(&pool).await, 1);
    let reason: String = sqlx::query_scalar(
        "select payload->>'reason' from tml_switchboard.audit_events \
         where event_type = 'registration_denied.v1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reason, "no_verified_primary_email");
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

    // Drive login + callback. Admitted, so the callback stages the login with
    // ToS consent still required -- but creates NO user.
    let cb = drive_to_callback(&c, addr, &pool).await;
    assert_eq!(cb.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = cb.json().await.unwrap();
    assert_eq!(body["required"], serde_json::json!(["tos"]));
    assert_eq!(body["tos_version"], 1);
    assert!(
        !body["staged_secret"].as_str().unwrap().is_empty(),
        "the marker carries the one-time completion secret"
    );

    // Between callback and completion: no user yet, one staged registration,
    // and the staged row holds only a salted hash of the secret.
    assert_eq!(
        octocat_user_count(&pool).await,
        0,
        "no user record before ToS acceptance"
    );
    assert_eq!(staged_count(&pool).await, 1, "one staged login");
    let stored_hash: String =
        sqlx::query_scalar("select secret_hash from tml_switchboard.staged_logins")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        stored_hash.starts_with("$argon2id$"),
        "secret stored as a salted argon2id PHC string, got {stored_hash:?}"
    );
    assert!(!stored_hash.contains(body["staged_secret"].as_str().unwrap()));

    // Complete the login: the user is created at the accepted version, a token
    // is returned, and the staged row is consumed.
    let complete = complete_login(&c, addr, &body).await;
    assert_eq!(complete.status(), reqwest::StatusCode::OK);
    let _session: LoginResponse = complete.json().await.unwrap();

    assert_eq!(
        octocat_user_count(&pool).await,
        1,
        "user created on ToS acceptance"
    );
    assert_eq!(octocat_tos_version(&pool).await, Some(1));
    assert_eq!(
        staged_count(&pool).await,
        0,
        "the staged registration was consumed"
    );

    // The staged pair is single-use: a second completion is 410 Gone.
    let replay = complete_login(&c, addr, &body).await;
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
    // the staging requires re-consent, with the token withheld and NO
    // duplicate record.
    let cb = drive_to_callback(&c, addr_v2, &pool).await;
    assert_eq!(cb.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = cb.json().await.unwrap();
    assert_eq!(body["required"], serde_json::json!(["tos"]));
    assert_eq!(body["tos_version"], 2);

    // Still exactly one user, still recorded at v1 (no token issued yet).
    assert_eq!(
        octocat_user_count(&pool).await,
        1,
        "no duplicate record for the re-accepting user"
    );
    assert_eq!(octocat_tos_version(&pool).await, Some(1));

    // Accept v2: a token is issued, the accepted version advances, and the user
    // is still a single record.
    let complete = complete_login(&c, addr_v2, &body).await;
    assert_eq!(complete.status(), reqwest::StatusCode::OK);
    let _session: LoginResponse = complete.json().await.unwrap();

    assert_eq!(
        octocat_user_count(&pool).await,
        1,
        "still no duplicate record"
    );
    assert_eq!(octocat_tos_version(&pool).await, Some(2));
    assert_eq!(staged_count(&pool).await, 0);
}

/// Parse a redirect's Location, asserting it points at the console's landing
/// URL and carries ONLY the staged pair (never a token, never the secret's
/// context) in the query; returns the pair.
fn landing_pair(resp: &reqwest::Response) -> (String, String) {
    assert!(
        resp.status().is_redirection(),
        "expected a redirect to the console landing, got {}",
        resp.status()
    );
    let location = url::Url::parse(resp.headers()["location"].to_str().unwrap()).unwrap();
    assert_eq!(location.host_str(), Some("console.example"));
    assert_eq!(location.path(), "/auth/landing");
    let query: std::collections::HashMap<_, _> = location.query_pairs().collect();
    assert!(
        !query.contains_key("token") && !query.contains_key("expires_at"),
        "a session token must never transit a redirect URL, got {location}"
    );
    assert_eq!(query.len(), 2, "only the staged pair rides the redirect");
    (
        query["staged_id"].to_string(),
        query["staged_secret"].to_string(),
    )
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn browser_flow_completes_tos_via_form_post(pool: PgPool) {
    let gh = MockServer::start().await;
    mount_github(&gh, &[]).await;

    // The browser (console) configuration: its landing URL is allowlisted, and
    // the console declares it per-login as return_to.
    const LANDING: &str = "https://console.example/auth/landing";
    let mut cfg = test_config(&gh.uri());
    cfg.oauth.return_to_allowlist = vec![LANDING.to_string()];
    let addr = spawn_server(AppState::new(pool.clone(), cfg)).await;

    sqlx::query(
        "insert into tml_switchboard.login_allowlist (provider, kind, external_id) \
         values ('github', 'user', '12345')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let c = client();

    // Initiate with the declared return_to, then drive the callback.
    let login_resp = c
        .get(format!("http://{addr}/api/v1/auth/github/login"))
        .query(&[("return_to", LANDING)])
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

    // The callback 302s the browser to the console's landing page carrying
    // only the single-use staged pair; no user exists yet.
    let (staged_id, staged_secret) = landing_pair(&cb);
    assert_eq!(octocat_user_count(&pool).await, 0, "no user yet");

    // The console exchanges the pair server-to-server (JSON), declaring no ToS
    // consent: the pair is consumed and a fresh one comes back as the 409
    // marker for the console's consent form to embed.
    let exchange = c
        .post(format!("http://{addr}/api/v1/auth/login/complete"))
        .json(&serde_json::json!({
            "staged_id": staged_id,
            "staged_secret": staged_secret,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(exchange.status(), reqwest::StatusCode::CONFLICT);
    let marker: serde_json::Value = exchange.json().await.unwrap();
    assert_eq!(marker["required"], serde_json::json!(["tos"]));
    assert_eq!(marker["tos_version"], 1);
    assert_eq!(octocat_user_count(&pool).await, 0, "still no user");

    // The console's no-JS consent form POSTs the fresh pair back form-encoded;
    // the completion provisions the user and 302s the browser back to the
    // landing page with a fresh ready-to-claim pair -- never the token.
    let complete = c
        .post(format!("http://{addr}/api/v1/auth/login/complete"))
        .form(&[
            ("staged_id", marker["staged_id"].as_str().unwrap()),
            ("staged_secret", marker["staged_secret"].as_str().unwrap()),
            ("tos_version", "1"),
        ])
        .send()
        .await
        .unwrap();
    let (claim_id, claim_secret) = landing_pair(&complete);
    assert_eq!(octocat_user_count(&pool).await, 1, "user provisioned");
    assert_eq!(octocat_tos_version(&pool).await, Some(1));

    // The console exchanges the ready-to-claim pair for the session token.
    let claim = c
        .post(format!("http://{addr}/api/v1/auth/login/complete"))
        .json(&serde_json::json!({
            "staged_id": claim_id,
            "staged_secret": claim_secret,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(claim.status(), reqwest::StatusCode::OK);
    let _session: LoginResponse = claim.json().await.unwrap();
    assert_eq!(staged_count(&pool).await, 0);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn non_allowlisted_return_to_is_rejected(pool: PgPool) {
    let gh = MockServer::start().await;
    mount_github(&gh, &[]).await;
    let mut cfg = test_config(&gh.uri());
    cfg.oauth.return_to_allowlist = vec!["https://console.example/auth/landing".to_string()];
    let addr = spawn_server(AppState::new(pool.clone(), cfg)).await;

    // The staged pair a return_to receives can mint a session token, so a
    // value outside the allowlist must be refused up front, before any flow
    // state exists.
    for target in [
        "https://evil.example/auth/landing",
        "https://console.example/auth/landing/../evil",
        "https://console.example",
    ] {
        let resp = client()
            .get(format!("http://{addr}/api/v1/auth/github/login"))
            .query(&[("return_to", target)])
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::BAD_REQUEST,
            "return_to {target:?} must be rejected"
        );
    }

    let flows: i64 = sqlx::query_scalar("select count(*) from tml_switchboard.oauth_flows")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(flows, 0, "a rejected login must not start a flow");
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn wrong_staged_secret_neither_completes_nor_burns(pool: PgPool) {
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
    let cb = drive_to_callback(&c, addr, &pool).await;
    assert_eq!(cb.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = cb.json().await.unwrap();

    // The staged id alone is no capability: completing with the right id but
    // a wrong secret is 410, creates nothing, and -- crucially -- does NOT
    // consume the staged row (an id-only caller cannot burn the login).
    let forged = c
        .post(format!("http://{addr}/api/v1/auth/login/complete"))
        .json(&serde_json::json!({
            "staged_id": body["staged_id"],
            "staged_secret": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "tos_version": body["tos_version"],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), reqwest::StatusCode::GONE);
    assert_eq!(octocat_user_count(&pool).await, 0, "no user provisioned");
    assert_eq!(
        staged_count(&pool).await,
        1,
        "a wrong secret must not burn the staged login"
    );

    // The legitimate holder of the pair still completes normally.
    let complete = complete_login(&c, addr, &body).await;
    assert_eq!(complete.status(), reqwest::StatusCode::OK);
    assert_eq!(octocat_user_count(&pool).await, 1);
    assert_eq!(staged_count(&pool).await, 0);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn expired_staged_login_cannot_complete(pool: PgPool) {
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
    let cb = drive_to_callback(&c, addr, &pool).await;
    assert_eq!(cb.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = cb.json().await.unwrap();

    // The pair expires before the user completes: presenting it is uniformly a
    // 410, provisioning nothing. (The now-inert row lingers until the next
    // staging's housekeeping sweep — the expiry check on consumption is what
    // guarantees correctness.)
    sqlx::query(
        "update tml_switchboard.staged_logins set expires_at = now() - interval '1 second'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let complete = complete_login(&c, addr, &body).await;
    assert_eq!(complete.status(), reqwest::StatusCode::GONE);
    assert_eq!(octocat_user_count(&pool).await, 0, "no user provisioned");

    // Replay stays dead.
    let replay = complete_login(&c, addr, &body).await;
    assert_eq!(replay.status(), reqwest::StatusCode::GONE);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn stale_tos_version_echo_is_not_recorded(pool: PgPool) {
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
    let cb = drive_to_callback(&c, addr, &pool).await;
    assert_eq!(cb.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = cb.json().await.unwrap();

    // Echoing a ToS version other than the one in force (as after a concurrent
    // bump) must not record consent: the presented pair is consumed and the
    // marker re-offers the current version with a FRESH pair for a corrected
    // retry.
    let stale = c
        .post(format!("http://{addr}/api/v1/auth/login/complete"))
        .json(&serde_json::json!({
            "staged_id": body["staged_id"],
            "staged_secret": body["staged_secret"],
            "tos_version": 999,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), reqwest::StatusCode::CONFLICT);
    let retry: serde_json::Value = stale.json().await.unwrap();
    assert_eq!(
        retry["tos_version"], 1,
        "marker re-offers the current version"
    );
    assert_ne!(
        retry["staged_secret"], body["staged_secret"],
        "the marker carries a fresh pair"
    );
    assert_eq!(octocat_user_count(&pool).await, 0, "no user provisioned");
    assert_eq!(staged_count(&pool).await, 1, "consumed and re-staged");

    // The presented pair was consumed by the stale echo; only the fresh one
    // completes.
    let replay = complete_login(&c, addr, &body).await;
    assert_eq!(replay.status(), reqwest::StatusCode::GONE);
    let complete = complete_login(&c, addr, &retry).await;
    assert_eq!(complete.status(), reqwest::StatusCode::OK);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn account_locked_after_staging_is_refused_completion(pool: PgPool) {
    let gh = MockServer::start().await;
    mount_github(&gh, &[]).await;

    sqlx::query(
        "insert into tml_switchboard.login_allowlist (provider, kind, external_id) \
         values ('github', 'user', '12345')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Register + accept at v1, then stage a re-acceptance against a v2 server.
    let addr_v1 = spawn_server(AppState::new(pool.clone(), test_config(&gh.uri()))).await;
    let c = client();
    assert_eq!(
        drive_full_login(&c, addr_v1, &pool).await,
        reqwest::StatusCode::OK
    );
    let mut cfg = test_config(&gh.uri());
    cfg.service.current_tos_version = 2;
    let addr_v2 = spawn_server(AppState::new(pool.clone(), cfg)).await;
    let cb = drive_to_callback(&c, addr_v2, &pool).await;
    assert_eq!(cb.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = cb.json().await.unwrap();

    // The account gets locked in the window between staging and completion.
    sqlx::query(
        "update tml_switchboard.users set locked = true \
         from tml_switchboard.user_identities i \
         where i.user_id = subject_id and i.provider_user_id = '12345'",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Completion is refused like the callback would refuse a locked login: no
    // token, no recorded re-acceptance, and the same denial audit trail.
    let complete = complete_login(&c, addr_v2, &body).await;
    assert_eq!(complete.status(), reqwest::StatusCode::FORBIDDEN);
    assert_eq!(
        octocat_tos_version(&pool).await,
        Some(1),
        "no consent recorded for a locked account"
    );
    let denials: i64 = sqlx::query_scalar(
        "select count(*) from tml_switchboard.audit_events \
         where event_type = 'login_denied_locked.v1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(denials, 1, "locked-login denial recorded");
    assert_eq!(staged_count(&pool).await, 0, "the staged login is consumed");

    // Replaying the pair after the denial stays dead.
    let replay = complete_login(&c, addr_v2, &body).await;
    assert_eq!(replay.status(), reqwest::StatusCode::GONE);
}

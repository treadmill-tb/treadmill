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
    drive_to_callback(client, addr, pool).await.status()
}

/// Start a login flow and drive the callback, returning the raw callback
/// response (so callers can inspect the `409` login-incomplete marker).
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

/// POST the staged pair from a `409` marker `body` to `/auth/login/complete`,
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

/// Drive a full interactive login, transparently finishing the completion step
/// (ToS accept) when the callback returns it. Returns the final HTTP status:
/// the callback's status when the login needed no completion (e.g. a `403`
/// denial, or an existing user with a current ToS), otherwise the
/// `/auth/login/complete` status.
async fn drive_full_login(
    client: &reqwest::Client,
    addr: SocketAddr,
    pool: &PgPool,
) -> reqwest::StatusCode {
    let cb = drive_to_callback(client, addr, pool).await;
    if cb.status() != reqwest::StatusCode::CONFLICT {
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

    // Drive login + callback. Admitted, so the callback returns the 409
    // login-incomplete marker and stages a staged login -- but creates NO user.
    let cb = drive_to_callback(&c, addr, &pool).await;
    assert_eq!(cb.status(), reqwest::StatusCode::CONFLICT);
    let body: serde_json::Value = cb.json().await.unwrap();
    assert_eq!(body["login_incomplete"], true);
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
    // they are bounced through the interstitial with the token withheld and NO
    // duplicate record.
    let cb = drive_to_callback(&c, addr_v2, &pool).await;
    assert_eq!(cb.status(), reqwest::StatusCode::CONFLICT);
    let body: serde_json::Value = cb.json().await.unwrap();
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

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn browser_flow_completes_tos_via_form_post(pool: PgPool) {
    let gh = MockServer::start().await;
    mount_github(&gh, &[]).await;

    // The browser (console) configuration: both redirects set.
    let mut cfg = test_config(&gh.uri());
    cfg.oauth.browser_login_complete_redirect =
        Some("https://console.example/auth/complete".to_string());
    cfg.oauth.browser_success_redirect = Some("https://console.example/auth/landing".to_string());
    let addr = spawn_server(AppState::new(pool.clone(), cfg)).await;

    sqlx::query(
        "insert into tml_switchboard.login_allowlist (provider, kind, external_id) \
         values ('github', 'user', '12345')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let c = client();

    // The callback redirects the browser to the console's completion page,
    // carrying the staged pair in the query for its form to echo back.
    let cb = drive_to_callback(&c, addr, &pool).await;
    assert!(
        cb.status().is_redirection(),
        "browser gets a completion redirect"
    );
    let location = url::Url::parse(cb.headers()["location"].to_str().unwrap()).unwrap();
    assert_eq!(location.host_str(), Some("console.example"));
    assert_eq!(location.path(), "/auth/complete");
    let query: std::collections::HashMap<_, _> = location.query_pairs().collect();
    let staged_id = query["staged_id"].to_string();
    let staged_secret = query["staged_secret"].to_string();
    assert_eq!(query["tos_version"], "1");
    assert_eq!(octocat_user_count(&pool).await, 0, "no user yet");

    // The console's no-JS completion form POSTs the pair back form-encoded;
    // the login completes with a redirect to the console's landing page
    // carrying the token.
    let complete = c
        .post(format!("http://{addr}/api/v1/auth/login/complete"))
        .form(&[
            ("staged_id", staged_id.as_str()),
            ("staged_secret", staged_secret.as_str()),
            ("tos_version", "1"),
        ])
        .send()
        .await
        .unwrap();
    assert!(
        complete.status().is_redirection(),
        "completion redirects to the success landing, got {}",
        complete.status()
    );
    let landing = url::Url::parse(complete.headers()["location"].to_str().unwrap()).unwrap();
    assert_eq!(landing.host_str(), Some("console.example"));
    assert_eq!(landing.path(), "/auth/landing");
    let query: std::collections::HashMap<_, _> = landing.query_pairs().collect();
    assert!(!query["token"].is_empty());
    assert!(query.contains_key("expires_at"));

    assert_eq!(octocat_user_count(&pool).await, 1, "user provisioned");
    assert_eq!(octocat_tos_version(&pool).await, Some(1));
    assert_eq!(staged_count(&pool).await, 0);
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
    assert_eq!(cb.status(), reqwest::StatusCode::CONFLICT);
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
    assert_eq!(cb.status(), reqwest::StatusCode::CONFLICT);
    let body: serde_json::Value = cb.json().await.unwrap();

    // Echoing a ToS version other than the one in force (as after a concurrent
    // bump) must not record consent: the marker is re-issued with the current
    // version and the staged login survives for a corrected retry.
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
    assert_eq!(retry["login_incomplete"], true);
    assert_eq!(
        retry["tos_version"], 1,
        "marker re-offers the current version"
    );
    assert_eq!(octocat_user_count(&pool).await, 0, "no user provisioned");
    assert_eq!(staged_count(&pool).await, 1, "staged login not consumed");

    let complete = complete_login(&c, addr, &body).await;
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
    assert_eq!(cb.status(), reqwest::StatusCode::CONFLICT);
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

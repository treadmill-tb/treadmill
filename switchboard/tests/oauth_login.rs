//! End-to-end GitHub OAuth login test.
//!
//! Drives the real authorization-code flow against a `wiremock` server standing
//! in for GitHub: a real ephemeral Postgres (via `#[sqlx::test]`), the real
//! reqwest/JSON/transaction/reconcile code paths, and only GitHub's HTTP
//! responses faked.
//!
//! Queries here use sqlx's runtime API (not the `query!` macros) so the test
//! needs no entry in the offline `.sqlx` cache.

use std::net::SocketAddr;

use sqlx::PgPool;
use tokio::net::TcpListener;
use treadmill_rs::api::switchboard::{LoginResponse, WhoAmIResponse};
use treadmill_switchboard::routes::build_router;
use treadmill_switchboard::serve::AppState;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;
use common::test_config;

/// Mount the canned GitHub responses, with `org_ids` controlling which orgs the
/// user appears to be an active member of.
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
            { "email": "unverified@example.com", "verified": false, "primary": false },
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

/// Run one full login: start the flow (persisting the CSRF state), read that
/// state back out of the database, then drive the callback and return the
/// issued session.
async fn run_login(client: &reqwest::Client, addr: SocketAddr, pool: &PgPool) -> LoginResponse {
    let login_resp = client
        .get(format!("http://{addr}/api/v1/auth/github/login"))
        .send()
        .await
        .unwrap();
    assert!(
        login_resp.status().is_redirection(),
        "login should redirect to the provider, got {}",
        login_resp.status()
    );

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
    match cb_resp.status() {
        // Existing user with a current ToS: the callback logs them in directly.
        reqwest::StatusCode::OK => cb_resp.json().await.unwrap(),
        // A new (or stale-ToS) user is bounced through the completion step;
        // accept the ToS to complete the login and receive the session.
        reqwest::StatusCode::CONFLICT => {
            let body: serde_json::Value = cb_resp.json().await.unwrap();
            let complete = client
                .post(format!("http://{addr}/api/v1/auth/login/complete"))
                .json(&serde_json::json!({
                    "pending_id": body["pending_id"],
                    "pending_secret": body["pending_secret"],
                    "tos_version": body["tos_version"],
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(
                complete.status(),
                reqwest::StatusCode::OK,
                "completing the login (ToS accept) should succeed"
            );
            complete.json().await.unwrap()
        }
        other => panic!("unexpected callback status {other}"),
    }
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn github_login_provisions_and_reconciles(pool: PgPool) {
    let gh = MockServer::start().await;
    mount_github(&gh, &[42]).await;

    // The admission gate now guards new-user registration; allow-list the canned
    // "octocat" identity so it can register through the normal flow. (Org 42 is
    // an auto-group source below, a separate concern from admission.)
    sqlx::query(
        "insert into tml_switchboard.login_allowlist (provider, kind, external_id, comment) \
         values ('github', 'user', '12345', 'test fixture')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Seed a group with a GitHub-org auto-source for org id 42.
    let group_id = Uuid::new_v4();
    sqlx::query("insert into tml_switchboard.subjects (subject_id, kind) values ($1, 'group')")
        .bind(group_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("insert into tml_switchboard.groups (subject_id, name) values ($1, 'gh-org-42')")
        .bind(group_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "insert into tml_switchboard.group_auto_sources \
         (group_id, provider, external_id, membership_via) \
         values ($1, 'github', '42', 'github_org')",
    )
    .bind(group_id)
    .execute(&pool)
    .await
    .unwrap();

    let addr = spawn_server(AppState::new(pool.clone(), test_config(&gh.uri()))).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    // -- First login: provisions the user and joins the auto-group.
    let session = run_login(&client, addr, &pool).await;

    // A subject + user was created for the provider identity.
    let user_id: Uuid = sqlx::query_scalar(
        "select user_id from tml_switchboard.user_identities \
         where provider = 'github' and provider_user_id = '12345'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    let username: String =
        sqlx::query_scalar("select username from tml_switchboard.users where subject_id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(username, "octocat");

    // Verified tag of emails was recorded properly.
    let verified_emails: Vec<String> = sqlx::query_scalar(
        "select email from tml_switchboard.user_emails where verified = true and user_id = $1",
    )
    .bind(user_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(verified_emails, vec!["octo@example.com".to_string()]);
    let unverified_emails: Vec<String> = sqlx::query_scalar(
        "select email from tml_switchboard.user_emails where verified = false and user_id = $1",
    )
    .bind(user_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        unverified_emails,
        vec!["unverified@example.com".to_string()]
    );

    // The github_org membership was reconciled in.
    let auto_members: i64 = sqlx::query_scalar(
        "select count(*) from tml_switchboard.group_members \
         where group_id = $1 and member_id = $2 and source = 'github_org' and source_ref = '42'",
    )
    .bind(group_id)
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(auto_members, 1, "auto-group membership should be present");

    // The login wrote an attributable audit trail: every event carries the
    // immutable internal user_id as a relation, and the provider details and
    // resolved client address are recorded on the login marker.
    let event_types: Vec<String> = sqlx::query_scalar(
        "select e.event_type from tml_switchboard.audit_events e \
         join tml_switchboard.audit_event_relations r on r.event_id = e.event_id \
         where r.entity_kind = 'subject' and r.entity_id = $1 and r.role = 'subject' \
         order by e.event_type",
    )
    .bind(user_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    for expected in [
        "user_logged_in.v1",
        "user_provisioned.v1",
        "session_token_issued.v1",
    ] {
        assert!(
            event_types.iter().any(|t| t == expected),
            "expected a {expected} audit event about the user, got {event_types:?}",
        );
    }

    // The login marker carries the provider, provider-internal id, and a
    // populated client_ip resolved from the request.
    let login_payload: serde_json::Value = sqlx::query_scalar(
        "select e.payload from tml_switchboard.audit_events e \
         where e.event_type = 'user_logged_in.v1' and e.actor_id = $1 \
         order by e.created_at desc limit 1",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(login_payload["provider"], "github");
    assert_eq!(login_payload["provider_user_id"], "12345");
    assert_eq!(login_payload["user"], serde_json::json!(user_id));
    assert!(
        login_payload["client_ip"].is_string(),
        "client_ip should be recorded, got {}",
        login_payload["client_ip"],
    );

    // The issued token authenticates whoami.
    let who: WhoAmIResponse = client
        .get(format!("http://{addr}/api/v1/auth/whoami"))
        .bearer_auth(session.token.encode_for_http())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(who.user_id, user_id);
    assert_eq!(who.username, "octocat");
    assert_eq!(who.full_name.as_deref(), Some("The Octocat"));

    // Add a manual membership; reconciliation must never touch it.
    sqlx::query(
        "insert into tml_switchboard.group_members (group_id, member_id, source, source_ref) \
         values ($1, $2, 'manual', '')",
    )
    .bind(group_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    // -- Second login: the user has left org 42, so the auto membership is
    // reconciled away while the manual membership survives.
    gh.reset().await;
    mount_github(&gh, &[]).await;
    let _ = run_login(&client, addr, &pool).await;

    let auto_members: i64 = sqlx::query_scalar(
        "select count(*) from tml_switchboard.group_members \
         where group_id = $1 and member_id = $2 and source = 'github_org'",
    )
    .bind(group_id)
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(auto_members, 0, "auto-group membership should be removed");

    let manual_members: i64 = sqlx::query_scalar(
        "select count(*) from tml_switchboard.group_members \
         where group_id = $1 and member_id = $2 and source = 'manual'",
    )
    .bind(group_id)
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(manual_members, 1, "manual membership must be preserved");
}

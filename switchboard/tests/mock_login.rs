//! End-to-end test for the development-only mock OAuth provider.
//!
//! Unlike the GitHub flow, the mock provider talks to no external service: the
//! login route self-redirects to the callback with the chosen identity as the
//! `code`. So this test needs no wiremock — just a real ephemeral Postgres (via
//! `#[sqlx::test]`) and the real router. It exercises the full path: CSRF flow,
//! callback, provisioning, and the staged-login claim that mints the token —
//! and pins the invariant that logging in never confers admin; that membership
//! is only ever granted externally.
//!
//! Queries here use sqlx's runtime API (not the `query!` macros) so the test
//! needs no entry in the offline `.sqlx` cache.

use reqwest::redirect::Policy;
use sqlx::PgPool;
use uuid::Uuid;

use treadmill_switchboard::serve::AppState;

mod common;
use common::{grant_admin, run_mock_login, spawn_server, test_config_mock};

/// The seeded global admins group's well-known id (matches `ADMINS_GROUP_ID`).
const ADMINS_GROUP_ID: Uuid = Uuid::from_u128(1);

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
async fn mock_login_confers_no_admin(pool: PgPool) {
    let state = AppState::new(pool.clone(), test_config_mock());
    let addr = spawn_server(state).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    // Even alice — the roster's designated admin identity — gets nothing from
    // the login itself: admin is only ever provisioned externally.
    let (_session, who) = run_mock_login(&pool, &client, addr, "alice", true).await;
    assert_eq!(who.username, "alice");
    assert!(
        !is_global_admin(&pool, who.user_id).await,
        "login alone must never confer admin"
    );

    // A second login resolves the same user and still grants nothing.
    let (_again, who2) = run_mock_login(&pool, &client, addr, "alice", false).await;
    assert_eq!(who2.user_id, who.user_id, "re-login resolves the same user");
    assert!(
        !is_global_admin(&pool, who.user_id).await,
        "re-login must not confer admin either"
    );

    // The external grant is what makes her an admin …
    grant_admin(&pool, who.user_id).await;
    assert!(
        is_global_admin(&pool, who.user_id).await,
        "the external grant makes alice an admin"
    );

    // … and a further login leaves the membership untouched: exactly one row,
    // neither duplicated nor revoked.
    let (_third, _) = run_mock_login(&pool, &client, addr, "alice", false).await;
    let memberships: i64 = sqlx::query_scalar(
        "select count(*) from tml_switchboard.group_members \
         where group_id = $1 and member_id = $2",
    )
    .bind(ADMINS_GROUP_ID)
    .bind(who.user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        memberships, 1,
        "login must neither duplicate nor revoke an externally granted membership"
    );
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn admin_audit_feed_lists_each_event_once(pool: PgPool) {
    let state = AppState::new(pool.clone(), test_config_mock());
    let addr = spawn_server(state).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    // Two completed logins → two login events on alice's feed.
    let (session, who) = run_mock_login(&pool, &client, addr, "alice", true).await;
    let (_again, _) = run_mock_login(&pool, &client, addr, "alice", false).await;
    // The dedup below only bites for an admin viewer, who bypasses the
    // per-relation view-policy filter.
    grant_admin(&pool, who.user_id).await;

    // The audit feed must list each event once. The login events relate alice
    // twice (actor and subject), and an admin viewer bypasses the per-relation
    // view-policy filter, so both relation rows match the feed query — a plain
    // join here would repeat every such event.
    let feed: serde_json::Value = client
        .get(format!("http://{addr}/api/v1/users/{}/events", who.user_id))
        .bearer_auth(session.token.encode_for_http())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let events = feed["events"].as_array().expect("events array");
    let mut ids: Vec<&str> = events
        .iter()
        .map(|e| e["event_id"].as_str().expect("event_id string"))
        .collect();
    let total = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(
        ids.len(),
        total,
        "the feed must not repeat an event that carries multiple matching relations"
    );
    let logins = events
        .iter()
        .filter(|e| e["event_type"] == "user_logged_in.v1")
        .count();
    assert_eq!(logins, 2, "exactly one login event per completed login");
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

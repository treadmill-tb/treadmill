//! Route tests for `GET /hosts` (the read-only host listing).
//!
//! Drives the real router over a loopback socket against ephemeral Postgres,
//! using the development mock-OAuth provider to obtain an authenticated caller
//! (no external service). Seeds a host and a target directly, then asserts the
//! listing exposes the host's tags, targets, and liveness.
//!
//! Queries here use sqlx's runtime API (not the `query!` macros), so the test
//! needs no entry in the offline `.sqlx` cache.

use std::sync::Arc;

use sqlx::PgPool;
use uuid::Uuid;

use treadmill_rs::api::switchboard::hosts::HostInfo;
use treadmill_switchboard::registry::OciRegistryClient;
use treadmill_switchboard::serve::AppState;

mod common;
use common::{mock_login_token, spawn_server, test_config_mock};

fn test_state(pool: PgPool) -> AppState {
    AppState::with_components(
        pool,
        test_config_mock(),
        Arc::new(OciRegistryClient::new()),
        None,
    )
}

/// Insert a live host (heartbeat now) with `tags`, plus one target `dut0` with
/// `target_tags`. Returns the host id. Uses the runtime query API, so no
/// `.sqlx` entry is needed.
async fn seed_live_host(pool: &PgPool, name: &str, tags: &[&str], target_tags: &[&str]) -> Uuid {
    let host_id = Uuid::new_v4();
    let tags: Vec<String> = tags.iter().map(|s| s.to_string()).collect();
    // A unique 32-byte auth token (the column is `unique`); the first bytes
    // encode the host id so concurrent seeds in one test never collide.
    let mut auth_token = vec![0u8; 32];
    auth_token[..16].copy_from_slice(host_id.as_bytes());

    sqlx::query(
        "insert into tml_switchboard.hosts \
           (host_id, name, auth_token, tags, ssh_endpoints, last_seen_at) \
         values ($1, $2, $3, $4, '{}'::tml_switchboard.ssh_endpoint[], now())",
    )
    .bind(host_id)
    .bind(name)
    .bind(auth_token)
    .bind(&tags)
    .execute(pool)
    .await
    .unwrap();

    let target_tags: Vec<String> = target_tags.iter().map(|s| s.to_string()).collect();
    sqlx::query(
        "insert into tml_switchboard.host_targets (target_id, host_id, name, tags) \
         values ($1, $2, 'dut0', $3)",
    )
    .bind(Uuid::new_v4())
    .bind(host_id)
    .bind(&target_tags)
    .execute(pool)
    .await
    .unwrap();

    host_id
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn lists_hosts_with_tags_targets_and_liveness(pool: PgPool) {
    let host_id = seed_live_host(
        &pool,
        "rpi-lab-03",
        &["arch=arm64", "board=rpi4"],
        &["dut=nrf52"],
    )
    .await;

    let addr = spawn_server(test_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    // Any authenticated user may list; `bob` is a plain (non-admin) user.
    let token = mock_login_token(&pool, &client, addr, "bob", true).await;

    let resp = client
        .get(format!("http://{addr}/api/v1/hosts"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let hosts: Vec<HostInfo> = resp.json().await.unwrap();
    let host = hosts
        .iter()
        .find(|h| h.host_id == host_id)
        .expect("seeded host present in listing");

    assert_eq!(host.name, "rpi-lab-03");
    assert_eq!(host.tags, vec!["arch=arm64", "board=rpi4"]);
    assert!(host.live, "a host with a fresh heartbeat is live");
    assert!(host.last_seen_at.is_some());
    assert_eq!(host.targets.len(), 1);
    assert_eq!(host.targets[0].name, "dut0");
    assert_eq!(host.targets[0].tags, vec!["dut=nrf52"]);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn listing_hosts_requires_authentication(pool: PgPool) {
    let addr = spawn_server(test_state(pool)).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let resp = client
        .get(format!("http://{addr}/api/v1/hosts"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

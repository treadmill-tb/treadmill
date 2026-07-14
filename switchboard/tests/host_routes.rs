//! Route tests for `GET /hosts` (the read-only host listing).
//!
//! Drives the real router over a loopback socket against ephemeral Postgres,
//! using the development mock-OAuth provider to obtain an authenticated caller
//! (no external service). Seeds a host and a target directly, then asserts the
//! listing exposes the host's tags, targets, and liveness.
//!
//! Queries here use sqlx's runtime API (not the `query!` macros), so the test
//! needs no entry in the offline `.sqlx` cache.

use std::net::SocketAddr;
use std::sync::Arc;

use sqlx::PgPool;
use uuid::Uuid;

use treadmill_rs::api::switchboard::WhoAmIResponse;
use treadmill_rs::api::switchboard::hosts::HostInfo;
use treadmill_switchboard::events::EventBus;
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
        EventBus::default(),
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

/// An [`AppState`] whose event bus is fed by a live `tml_events` listener on the
/// per-test database, so DB writes produce SSE pings end to end.
fn watch_state(pool: PgPool) -> AppState {
    let bus = EventBus::default();
    tokio::spawn(bus.listener(pool.clone()));
    AppState::with_components(
        pool,
        test_config_mock(),
        Arc::new(OciRegistryClient::new()),
        None,
        bus,
    )
}

/// The authenticated caller's own `user_id`, via `GET /auth/whoami`.
async fn whoami(client: &reqwest::Client, addr: SocketAddr, token: &str) -> Uuid {
    let resp = client
        .get(format!("http://{addr}/api/v1/auth/whoami"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    resp.json::<WhoAmIResponse>().await.unwrap().user_id
}

/// Insert a host owned by `owner`. Returns the host id.
async fn seed_host_owned(pool: &PgPool, name: &str, owner: Uuid) -> Uuid {
    let host_id = Uuid::new_v4();
    let mut auth_token = vec![0u8; 32];
    auth_token[..16].copy_from_slice(host_id.as_bytes());
    sqlx::query(
        "insert into tml_switchboard.hosts \
           (host_id, name, auth_token, tags, ssh_endpoints, worker_instance_id, owner_id) \
         values ($1, $2, $3, '{}', '{}'::tml_switchboard.ssh_endpoint[], 0, $4)",
    )
    .bind(host_id)
    .bind(name)
    .bind(auth_token)
    .bind(owner)
    .execute(pool)
    .await
    .unwrap();
    host_id
}

/// Read body chunks until a full `change` event is seen. Panics on timeout or
/// stream end.
async fn next_change(resp: &mut reqwest::Response) {
    let mut buf = String::new();
    loop {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(10), resp.chunk())
            .await
            .expect("timed out waiting for an SSE frame")
            .expect("stream error")
            .expect("stream ended before a change event");
        buf.push_str(&String::from_utf8_lossy(&chunk));
        if buf.contains("event: change") {
            return;
        }
    }
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn watch_streams_host_changes(pool: PgPool) {
    let addr = spawn_server(watch_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let token = mock_login_token(&pool, &client, addr, "bob", true).await;
    let bob = whoami(&client, addr, &token).await;
    let host_id = seed_host_owned(&pool, "host-watch", bob).await;

    let mut resp = client
        .get(format!("http://{addr}/api/v1/hosts/{host_id}/watch"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream"),
    );

    // The subscription starts woken, so the stream pings immediately on open.
    next_change(&mut resp).await;

    // A change to a watched column of the host's row wakes the stream again.
    sqlx::query(
        "update tml_switchboard.hosts \
         set worker_instance_id = worker_instance_id + 1 where host_id = $1",
    )
    .bind(host_id)
    .execute(&pool)
    .await
    .unwrap();
    next_change(&mut resp).await;
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn watch_requires_read_access(pool: PgPool) {
    let addr = spawn_server(watch_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let bob_token = mock_login_token(&pool, &client, addr, "bob", true).await;
    let bob = whoami(&client, addr, &bob_token).await;
    let host_id = seed_host_owned(&pool, "host-watch-auth", bob).await;

    // A user with neither ownership nor a grant is refused (403, not a leak).
    let carol_token = mock_login_token(&pool, &client, addr, "carol", true).await;
    let carol = whoami(&client, addr, &carol_token).await;
    let refused = client
        .get(format!("http://{addr}/api/v1/hosts/{host_id}/watch"))
        .bearer_auth(&carol_token)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), reqwest::StatusCode::FORBIDDEN);

    // With an explicit `read` grant, the same user gets the stream.
    sqlx::query(
        "insert into tml_switchboard.host_grants (host_id, subject_id, permission) \
         values ($1, $2, 'read')",
    )
    .bind(host_id)
    .bind(carol)
    .execute(&pool)
    .await
    .unwrap();
    let mut granted = client
        .get(format!("http://{addr}/api/v1/hosts/{host_id}/watch"))
        .bearer_auth(&carol_token)
        .send()
        .await
        .unwrap();
    assert_eq!(granted.status(), reqwest::StatusCode::OK);
    next_change(&mut granted).await;
}

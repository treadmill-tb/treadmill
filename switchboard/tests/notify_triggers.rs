//! DB-backed tests for the `tml_events` change-notification triggers
//! (`notify_change()`, `jobs_notify`, `hosts_notify` — see the CHANGE
//! NOTIFICATIONS section of SCHEMA.sql for the payload contract).
//!
//! Each test opens a raw `PgListener` on the per-test database and asserts on
//! the exact sequence of received payloads: single-statement writes commit in
//! order, so the notification order is deterministic. "Emits nothing" is
//! proven by a marker write whose event must be the next one received.

use std::time::Duration;

use serde_json::Value;
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use treadmill_rs::image::{Digest, media_types};
use treadmill_switchboard::events::{EventBus, EventFilter};
use treadmill_switchboard::sql;
use uuid::Uuid;

async fn listen(pool: &PgPool) -> PgListener {
    let mut listener = PgListener::connect_with(pool).await.unwrap();
    listener.listen("tml_events").await.unwrap();
    listener
}

async fn next_event(listener: &mut PgListener) -> Value {
    let notification = tokio::time::timeout(Duration::from_secs(10), listener.recv())
        .await
        .expect("timed out waiting for a tml_events notification")
        .expect("listener connection failed");
    serde_json::from_str(notification.payload()).expect("payload must be JSON")
}

/// The values of one routing key, sorted for order-insensitive comparison.
fn key_values(event: &Value, column: &str) -> Vec<String> {
    let mut values: Vec<String> = event["keys"][column]
        .as_array()
        .unwrap_or_else(|| panic!("no key {column} in {event}"))
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    values.sort();
    values
}

async fn insert_user(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("insert into tml_switchboard.subjects (subject_id, kind) values ($1, 'user')")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("insert into tml_switchboard.users (subject_id, name) values ($1, $2)")
        .bind(id)
        .bind(format!("user-{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn insert_token(pool: &PgPool, user_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    let mut token = vec![0u8; 32];
    token[..16].copy_from_slice(id.as_bytes());
    sqlx::query(
        "insert into tml_switchboard.api_tokens \
         (token_id, token, user_id, revoked, created_at, expires_at) \
         values ($1, $2, $3, null, now(), now() + interval '1 day')",
    )
    .bind(id)
    .bind(token)
    .bind(user_id)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn insert_host(pool: &PgPool, owner: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    let mut auth_token = vec![0u8; 32];
    auth_token[..16].copy_from_slice(id.as_bytes());
    sqlx::query(
        "insert into tml_switchboard.hosts \
         (host_id, name, auth_token, tags, ssh_endpoints, worker_instance_id, owner_id) \
         values ($1, $2, $3, '{}', '{}'::tml_switchboard.ssh_endpoint[], 0, $4)",
    )
    .bind(id)
    .bind(format!("host-{id}"))
    .bind(auth_token)
    .bind(owner)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn insert_image(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    let digest = {
        let mut seed = [0u8; 32];
        seed[..16].copy_from_slice(id.as_bytes());
        Digest::from_sha256(seed)
    };
    sql::image::insert(
        pool,
        id,
        &digest.encoded(),
        media_types::IMAGE_ARTIFACT_TYPE,
        None,
    )
    .await
    .unwrap();
    id
}

async fn insert_job(pool: &PgPool, owner: Uuid, token: Uuid, image: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "insert into tml_switchboard.jobs \
         (job_id, owner_id, image_id, ssh_keys, restart_policy, enqueued_by_token_id, \
          host_tag_requirements, job_timeout, job_state, queued_at) \
         values ($1, $2, $3, '{}', row(0)::tml_switchboard.restart_policy, $4, '{}', \
                 interval '1 hour', 'queued', now())",
    )
    .bind(id)
    .bind(owner)
    .bind(image)
    .bind(token)
    .execute(pool)
    .await
    .unwrap();
    id
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn jobs_changes_emit_routing_keys(pool: PgPool) {
    let user = insert_user(&pool).await;
    let token = insert_token(&pool, user).await;
    let image = insert_image(&pool).await;

    let mut listener = listen(&pool).await;

    let host_a = insert_host(&pool, user).await;
    let host_b = insert_host(&pool, user).await;
    for expected in [host_a, host_b] {
        let event = next_event(&mut listener).await;
        assert_eq!(event["table"], "hosts");
        assert_eq!(key_values(&event, "host_id"), vec![expected.to_string()]);
    }

    // Insert: the pk is a key; the NULL dispatched_on_host_id is omitted.
    let job = insert_job(&pool, user, token, image).await;
    let event = next_event(&mut listener).await;
    assert_eq!(event["table"], "jobs");
    assert_eq!(key_values(&event, "job_id"), vec![job.to_string()]);
    assert!(event["keys"].get("dispatched_on_host_id").is_none());

    // Assignment: the new host id appears as a routing key.
    sqlx::query(
        "update tml_switchboard.jobs \
         set job_state = 'assigned', dispatched_on_host_id = $2 where job_id = $1",
    )
    .bind(job)
    .bind(host_a)
    .execute(&pool)
    .await
    .unwrap();
    let event = next_event(&mut listener).await;
    assert_eq!(
        key_values(&event, "dispatched_on_host_id"),
        vec![host_a.to_string()]
    );

    // Changing a routing-key column wakes both the old and the new scope.
    sqlx::query("update tml_switchboard.jobs set dispatched_on_host_id = $2 where job_id = $1")
        .bind(job)
        .bind(host_b)
        .execute(&pool)
        .await
        .unwrap();
    let event = next_event(&mut listener).await;
    let mut expected = vec![host_a.to_string(), host_b.to_string()];
    expected.sort();
    assert_eq!(key_values(&event, "dispatched_on_host_id"), expected);

    // A write that changes nothing is silent (proven by the delete event
    // arriving next).
    sqlx::query("update tml_switchboard.jobs set job_state = job_state where job_id = $1")
        .bind(job)
        .execute(&pool)
        .await
        .unwrap();

    // Delete: keys come from the OLD row.
    sqlx::query("delete from tml_switchboard.jobs where job_id = $1")
        .bind(job)
        .execute(&pool)
        .await
        .unwrap();
    let event = next_event(&mut listener).await;
    assert_eq!(event["table"], "jobs");
    assert_eq!(key_values(&event, "job_id"), vec![job.to_string()]);
    assert_eq!(
        key_values(&event, "dispatched_on_host_id"),
        vec![host_b.to_string()]
    );
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn host_heartbeats_stay_silent(pool: PgPool) {
    let user = insert_user(&pool).await;
    let mut listener = listen(&pool).await;

    let host = insert_host(&pool, user).await;
    let event = next_event(&mut listener).await;
    assert_eq!(event["table"], "hosts");
    assert_eq!(key_values(&event, "host_id"), vec![host.to_string()]);

    // Heartbeat refresh and clean-disconnect (`last_seen_at` only): no events.
    sqlx::query("update tml_switchboard.hosts set last_seen_at = now() where host_id = $1")
        .bind(host)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("update tml_switchboard.hosts set last_seen_at = null where host_id = $1")
        .bind(host)
        .execute(&pool)
        .await
        .unwrap();

    // A statement touching a notifying column without changing the row is
    // equally silent.
    sqlx::query("update tml_switchboard.hosts set tags = tags where host_id = $1")
        .bind(host)
        .execute(&pool)
        .await
        .unwrap();

    // Worker takeover (connect edge) does notify — and being the very next
    // event received, it proves the heartbeat writes above emitted nothing.
    sqlx::query(
        "update tml_switchboard.hosts \
         set worker_instance_id = worker_instance_id + 1 where host_id = $1",
    )
    .bind(host)
    .execute(&pool)
    .await
    .unwrap();
    let event = next_event(&mut listener).await;
    assert_eq!(event["table"], "hosts");
    assert_eq!(key_values(&event, "host_id"), vec![host.to_string()]);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn event_bus_listener_delivers_wakes(pool: PgPool) {
    let user = insert_user(&pool).await;
    let host = insert_host(&pool, user).await;

    let bus = EventBus::default();
    tokio::spawn(bus.listener(pool.clone()));
    let mut subscription = bus.subscribe(EventFilter {
        table: "hosts",
        key: Some(("host_id", host)),
    });
    subscription.changed().await;

    // The listener may not have connected yet when the first write commits (a
    // wake missed for that reason is what consumers' timers cover); retry the
    // write until a wake arrives.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        sqlx::query(
            "update tml_switchboard.hosts set tags = array[md5(random()::text)] where host_id = $1",
        )
        .bind(host)
        .execute(&pool)
        .await
        .unwrap();
        if tokio::time::timeout(Duration::from_millis(500), subscription.changed())
            .await
            .is_ok()
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no wake delivered through the event bus"
        );
    }
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn bulk_update_folds_into_one_keyless_event(pool: PgPool) {
    let user = insert_user(&pool).await;
    let hosts = [
        insert_host(&pool, user).await,
        insert_host(&pool, user).await,
        insert_host(&pool, user).await,
        insert_host(&pool, user).await,
    ];

    let mut listener = listen(&pool).await;

    let mut txn = pool.begin().await.unwrap();
    sqlx::query("set local tml_switchboard.notify_row_limit = '2'")
        .execute(&mut *txn)
        .await
        .unwrap();
    sqlx::query("update tml_switchboard.hosts set tags = array['bulk']")
        .execute(&mut *txn)
        .await
        .unwrap();
    txn.commit().await.unwrap();

    // Rows up to the limit emit per-row events; everything beyond folds into
    // identical keyless payloads that Postgres dedups to a single delivery.
    for _ in 0..2 {
        let event = next_event(&mut listener).await;
        assert_eq!(event["table"], "hosts");
        let values = key_values(&event, "host_id");
        assert_eq!(values.len(), 1);
        assert!(hosts.iter().any(|h| h.to_string() == values[0]));
    }
    let event = next_event(&mut listener).await;
    assert_eq!(event["table"], "hosts");
    assert!(event.get("keys").is_none());

    // A marker write must be the very next event: the bulk transaction
    // produced nothing further.
    sqlx::query("update tml_switchboard.hosts set tags = array['marker'] where host_id = $1")
        .bind(hosts[0])
        .execute(&pool)
        .await
        .unwrap();
    let event = next_event(&mut listener).await;
    assert_eq!(key_values(&event, "host_id"), vec![hosts[0].to_string()]);
}

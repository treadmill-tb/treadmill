//! Route tests for `POST /jobs/{id}/log-token` (NATS read-token minting).
//!
//! Drives the real router over a loopback socket against ephemeral Postgres,
//! using the development mock-OAuth provider to obtain an authenticated caller
//! (no external service). `alice` is provisioned as a global admin (so she can
//! read any job — no job row needs to exist), `bob` is a plain user. No live
//! NATS is involved: minting the token is pure (the wire round-trip lives in
//! the `nats-log-streaming` Nix check).
//!
//! Queries here use sqlx's runtime API (not the `query!` macros), so the test
//! needs no entry in the offline `.sqlx` cache.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::redirect::Policy;
use sqlx::PgPool;
use tokio::net::TcpListener;
use uuid::Uuid;

use treadmill_rs::api::switchboard::audit::AuditFeedResponse;
use treadmill_rs::api::switchboard::jobs::{
    EnqueueJobResponse, JobImageRef, JobInfo, JobListResponse, LogStreamCredentials,
};
use treadmill_rs::api::switchboard::{
    JobInitSpec, JobRequest, JobState, LoginResponse, WhoAmIResponse,
};
use treadmill_rs::api::switchboard_supervisor::RestartPolicy;

/// The built-in admins group subject (`engine::ADMINS_GROUP_ID`). `alice` is a
/// member, so she may file a job under it.
const ADMINS_GROUP_ID: Uuid = Uuid::from_u128(1);
use treadmill_switchboard::config::LogStreamingConfig;
use treadmill_switchboard::log_streaming::{LogStreamProvisioner, LogStreaming, ProvisionError};
use treadmill_switchboard::registry::OciRegistryClient;
use treadmill_switchboard::routes::build_router;
use treadmill_switchboard::serve::AppState;

mod common;
use common::test_config_mock;

/// A provisioner the read route never calls (it only mints tokens); present
/// only to satisfy the `LogStreaming` struct.
struct NoopProvisioner;

#[async_trait::async_trait]
impl LogStreamProvisioner for NoopProvisioner {
    async fn ensure_job_stream(&self, _job_id: Uuid) -> Result<(), ProvisionError> {
        Ok(())
    }
}

/// A streaming-enabled `LogStreaming` with a throwaway account seed (generated
/// per run, so no real secret enters the source tree).
fn test_log_streaming() -> LogStreaming {
    let account = nats_jwt::KeyPair::new_account();
    LogStreaming {
        config: LogStreamingConfig {
            nats_url: "nats://nats.example:4222".to_string(),
            jetstream_domain: None,
            account_seed: account.seed().expect("account seed"),
        },
        provisioner: Arc::new(NoopProvisioner),
    }
}

fn streaming_enabled_state(pool: PgPool) -> AppState {
    AppState::with_components(
        pool,
        test_config_mock(),
        Arc::new(OciRegistryClient::new()),
        Some(test_log_streaming()),
    )
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

/// Drive a full mock login for `identity` and return the issued bearer token
/// (HTTP-encoded), ready for `bearer_auth`.
async fn mock_login_token(client: &reqwest::Client, addr: SocketAddr, identity: &str) -> String {
    let login = client
        .get(format!(
            "http://{addr}/api/v1/auth/mock/login?identity={identity}"
        ))
        .send()
        .await
        .unwrap();
    assert!(
        login.status().is_redirection(),
        "mock login should redirect"
    );
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
        "callback should succeed"
    );
    let session: LoginResponse = cb.json().await.unwrap();
    session.token.encode_for_http()
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

/// The most recently issued token id for `user_id` (the session minted by
/// `mock_login_token`), usable as a job's `enqueued_by_token_id`.
async fn latest_token_id(pool: &PgPool, user_id: Uuid) -> Uuid {
    sqlx::query_scalar(
        "select token_id from tml_switchboard.api_tokens \
         where user_id = $1 order by created_at desc limit 1",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Register a throwaway concrete image (unique digest, no location) and return
/// its catalog id. A job's `image_id` is an FK into `images`, so the seed/enqueue
/// helpers need a real row to reference. Uses the runtime query API, so no
/// `.sqlx` entry is needed.
async fn register_image(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    let id_hex = id.simple().to_string();
    let digest = format!("sha256:{id_hex:0>64}");
    sqlx::query(
        "insert into tml_switchboard.images \
           (id, manifest_digest, artifact_type, owner_subject, attrs) \
         values ($1, $2, 'application/vnd.treadmill.image.v1+json', null, '{}'::jsonb)",
    )
    .bind(id)
    .bind(digest)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// Insert a `queued`, concrete-image job owned by `owner`, with the given
/// parameters (`key`, `value`, `secret`). A fresh image is registered and
/// referenced by id. Returns the job id. Uses the runtime query API, so no
/// `.sqlx` entry is needed.
async fn seed_job(pool: &PgPool, owner: Uuid, token: Uuid, params: &[(&str, &str, bool)]) -> Uuid {
    let job_id = Uuid::new_v4();
    let image_id = register_image(pool).await;
    sqlx::query(
        "insert into tml_switchboard.jobs \
           (job_id, owner_id, image_id, ssh_keys, restart_policy, \
            enqueued_by_token_id, host_tag_requirements, job_timeout, \
            job_state, queued_at) \
         values ($1, $2, $3, '{}', row(0)::tml_switchboard.restart_policy, \
            $4, '{}', interval '1 hour', 'queued', now())",
    )
    .bind(job_id)
    .bind(owner)
    .bind(image_id)
    .bind(token)
    .execute(pool)
    .await
    .unwrap();

    for (key, value, secret) in params {
        sqlx::query(
            "insert into tml_switchboard.job_parameters (job_id, key, value) \
             values ($1, $2, row($3, $4)::tml_switchboard.parameter_value)",
        )
        .bind(job_id)
        .bind(key)
        .bind(value)
        .bind(secret)
        .execute(pool)
        .await
        .unwrap();
    }
    job_id
}

/// Insert a `queued` job owned by `owner` with an explicit `queued_at`, for
/// deterministic listing order. Returns the job id.
async fn seed_job_at(
    pool: &PgPool,
    owner: Uuid,
    token: Uuid,
    queued_at: chrono::DateTime<chrono::Utc>,
) -> Uuid {
    let job_id = Uuid::new_v4();
    let image_id = register_image(pool).await;
    sqlx::query(
        "insert into tml_switchboard.jobs \
           (job_id, owner_id, image_id, ssh_keys, restart_policy, \
            enqueued_by_token_id, host_tag_requirements, job_timeout, \
            job_state, queued_at) \
         values ($1, $2, $3, '{}', row(0)::tml_switchboard.restart_policy, \
            $4, '{}', interval '1 hour', 'queued', $5)",
    )
    .bind(job_id)
    .bind(owner)
    .bind(image_id)
    .bind(token)
    .bind(queued_at)
    .execute(pool)
    .await
    .unwrap();
    job_id
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn list_paginates_readable_jobs_newest_first(pool: PgPool) {
    let addr = spawn_server(streaming_enabled_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    let token = mock_login_token(&client, addr, "bob").await;
    let bob = whoami(&client, addr, &token).await;
    let token_id = latest_token_id(&pool, bob).await;

    // Three jobs at distinct, increasing queue times: j2 is newest.
    let base = chrono::Utc::now() - chrono::Duration::hours(1);
    let j0 = seed_job_at(&pool, bob, token_id, base).await;
    let j1 = seed_job_at(&pool, bob, token_id, base + chrono::Duration::minutes(1)).await;
    let j2 = seed_job_at(&pool, bob, token_id, base + chrono::Duration::minutes(2)).await;

    // First page of 2: newest first (j2, j1), with a cursor for more.
    let page1: JobListResponse = client
        .get(format!("http://{addr}/api/v1/jobs?limit=2"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids1: Vec<Uuid> = page1.jobs.iter().map(|j| j.job_id).collect();
    assert_eq!(ids1, vec![j2, j1]);
    let cursor = page1.next_cursor.expect("a further page exists");

    // Second page: the remaining job (j0), no further cursor.
    let page2: JobListResponse = client
        .get(format!("http://{addr}/api/v1/jobs?limit=2&cursor={cursor}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids2: Vec<Uuid> = page2.jobs.iter().map(|j| j.job_id).collect();
    assert_eq!(ids2, vec![j0]);
    assert!(page2.next_cursor.is_none());
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn list_scopes_to_readable_jobs(pool: PgPool) {
    let addr = spawn_server(streaming_enabled_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    // One job each for alice and bob.
    let alice_token = mock_login_token(&client, addr, "alice").await;
    let alice = whoami(&client, addr, &alice_token).await;
    let alice_tok = latest_token_id(&pool, alice).await;
    let alice_job = seed_job(&pool, alice, alice_tok, &[]).await;

    let bob_token = mock_login_token(&client, addr, "bob").await;
    let bob = whoami(&client, addr, &bob_token).await;
    let bob_tok = latest_token_id(&pool, bob).await;
    let bob_job = seed_job(&pool, bob, bob_tok, &[]).await;

    // `bob` sees only his own job.
    let bob_list: JobListResponse = client
        .get(format!("http://{addr}/api/v1/jobs"))
        .bearer_auth(&bob_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let bob_ids: Vec<Uuid> = bob_list.jobs.iter().map(|j| j.job_id).collect();
    assert_eq!(bob_ids, vec![bob_job]);

    // `alice`, a global admin, sees both.
    let alice_list: JobListResponse = client
        .get(format!("http://{addr}/api/v1/jobs"))
        .bearer_auth(&alice_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let alice_ids: Vec<Uuid> = alice_list.jobs.iter().map(|j| j.job_id).collect();
    assert!(alice_ids.contains(&alice_job));
    assert!(alice_ids.contains(&bob_job));
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn list_rejects_a_malformed_cursor(pool: PgPool) {
    let addr = spawn_server(streaming_enabled_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    let token = mock_login_token(&client, addr, "bob").await;
    let resp = client
        .get(format!(
            "http://{addr}/api/v1/jobs?cursor=not-a-valid-cursor"
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn enqueue_and_cancel_emit_audit_events(pool: PgPool) {
    let addr = spawn_server(streaming_enabled_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    let token = mock_login_token(&client, addr, "bob").await;

    // Enqueue a job, then cancel it (still queued, so it finalizes immediately).
    let image = register_image(&pool).await;
    let req = image_job_request(None, JobInitSpec::Image { image }, None);
    let job_id = client
        .post(format!("http://{addr}/api/v1/jobs"))
        .bearer_auth(&token)
        .json(&req)
        .send()
        .await
        .unwrap()
        .json::<EnqueueJobResponse>()
        .await
        .unwrap()
        .job_id;

    // The owner sees the enqueue in the job's audit feed (job-read visibility).
    let after_enqueue: AuditFeedResponse = client
        .get(format!("http://{addr}/api/v1/jobs/{job_id}/events"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let types: Vec<&str> = after_enqueue
        .events
        .iter()
        .map(|e| e.event_type.as_str())
        .collect();
    assert!(
        types.contains(&"job_enqueued.v1"),
        "expected a job_enqueued event, got {types:?}"
    );
    assert!(!types.contains(&"job_canceled.v1"));

    let cancel = client
        .delete(format!("http://{addr}/api/v1/jobs/{job_id}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(cancel.status(), reqwest::StatusCode::ACCEPTED);

    // The cancellation now shows up alongside the enqueue.
    let after_cancel: AuditFeedResponse = client
        .get(format!("http://{addr}/api/v1/jobs/{job_id}/events"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let types: Vec<&str> = after_cancel
        .events
        .iter()
        .map(|e| e.event_type.as_str())
        .collect();
    assert!(types.contains(&"job_enqueued.v1"), "got {types:?}");
    assert!(types.contains(&"job_canceled.v1"), "got {types:?}");
}

/// A minimal concrete-image [`JobRequest`] for enqueue tests.
fn image_job_request(
    owner: Option<Uuid>,
    init_spec: JobInitSpec,
    override_timeout: Option<chrono::Duration>,
) -> JobRequest {
    JobRequest {
        init_spec,
        owner,
        ssh_keys: vec![],
        restart_policy: RestartPolicy {
            remaining_restart_count: 0,
        },
        parameters: HashMap::new(),
        host_tag_requirements: vec![],
        target_requirements: vec![],
        override_timeout,
    }
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn enqueue_creates_a_queued_job_owned_by_caller(pool: PgPool) {
    let addr = spawn_server(streaming_enabled_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    let token = mock_login_token(&client, addr, "bob").await;
    let bob = whoami(&client, addr, &token).await;
    let image = register_image(&pool).await;
    let req = image_job_request(None, JobInitSpec::Image { image }, None);

    let resp = client
        .post(format!("http://{addr}/api/v1/jobs"))
        .bearer_auth(&token)
        .json(&req)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let job_id = resp.json::<EnqueueJobResponse>().await.unwrap().job_id;

    // The enqueuer owns the job and can read it back: queued, owned by the
    // caller, with the deployment default timeout (1h in the mock config).
    let info: JobInfo = client
        .get(format!("http://{addr}/api/v1/jobs/{job_id}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info.owner_id, Some(bob));
    assert_eq!(info.state, JobState::Queued);
    assert_eq!(info.timeout_secs, 3600);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn enqueue_under_a_group_the_caller_belongs_to(pool: PgPool) {
    let addr = spawn_server(streaming_enabled_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    // `alice` is a member of the admins group, so she may own the job under it.
    let token = mock_login_token(&client, addr, "alice").await;
    let image = register_image(&pool).await;
    let req = image_job_request(Some(ADMINS_GROUP_ID), JobInitSpec::Image { image }, None);

    let resp = client
        .post(format!("http://{addr}/api/v1/jobs"))
        .bearer_auth(&token)
        .json(&req)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let job_id = resp.json::<EnqueueJobResponse>().await.unwrap().job_id;

    let info: JobInfo = client
        .get(format!("http://{addr}/api/v1/jobs/{job_id}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info.owner_id, Some(ADMINS_GROUP_ID));
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn enqueue_with_unrelated_owner_is_forbidden(pool: PgPool) {
    let addr = spawn_server(streaming_enabled_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    // `bob` is not a member of any group, and certainly not of a random subject,
    // so he cannot file a job under it.
    let token = mock_login_token(&client, addr, "bob").await;
    // The unrelated-owner check rejects this (403) before the image is ever
    // looked up, so an unregistered image id is fine here.
    let req = image_job_request(
        Some(Uuid::new_v4()),
        JobInitSpec::Image {
            image: Uuid::new_v4(),
        },
        None,
    );

    let resp = client
        .post(format!("http://{addr}/api/v1/jobs"))
        .bearer_auth(&token)
        .json(&req)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn restarting_a_job_without_manage_is_forbidden(pool: PgPool) {
    let addr = spawn_server(streaming_enabled_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    // A job owned by `alice`; `bob` holds no permission on it.
    let alice_token = mock_login_token(&client, addr, "alice").await;
    let alice = whoami(&client, addr, &alice_token).await;
    let alice_tok = latest_token_id(&pool, alice).await;
    let existing = seed_job(&pool, alice, alice_tok, &[]).await;

    let bob_token = mock_login_token(&client, addr, "bob").await;
    let req = image_job_request(None, JobInitSpec::RestartJob { job_id: existing }, None);

    let resp = client
        .post(format!("http://{addr}/api/v1/jobs"))
        .bearer_auth(&bob_token)
        .json(&req)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn enqueue_honors_an_override_timeout(pool: PgPool) {
    let addr = spawn_server(streaming_enabled_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    let token = mock_login_token(&client, addr, "bob").await;
    let image = register_image(&pool).await;
    let req = image_job_request(
        None,
        JobInitSpec::Image { image },
        Some(chrono::Duration::hours(2)),
    );

    let resp = client
        .post(format!("http://{addr}/api/v1/jobs"))
        .bearer_auth(&token)
        .json(&req)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let job_id = resp.json::<EnqueueJobResponse>().await.unwrap().job_id;

    let info: JobInfo = client
        .get(format!("http://{addr}/api/v1/jobs/{job_id}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info.timeout_secs, 2 * 3600);
}

/// A job's `(job_state, termination_reason)` as enum text.
async fn job_state_and_reason(pool: &PgPool, job_id: Uuid) -> (String, Option<String>) {
    sqlx::query_as(
        "select job_state::text, termination_reason::text \
         from tml_switchboard.jobs where job_id = $1",
    )
    .bind(job_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn canceling_a_queued_job_finalizes_it(pool: PgPool) {
    let addr = spawn_server(streaming_enabled_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    // `bob` owns (and so may stop) a queued job.
    let token = mock_login_token(&client, addr, "bob").await;
    let bob = whoami(&client, addr, &token).await;
    let token_id = latest_token_id(&pool, bob).await;
    let job_id = seed_job(&pool, bob, token_id, &[]).await;

    let resp = client
        .delete(format!("http://{addr}/api/v1/jobs/{job_id}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    // Queued: finalized synchronously, so a cancellation was initiated (202).
    assert_eq!(resp.status(), reqwest::StatusCode::ACCEPTED);

    let (state, reason) = job_state_and_reason(&pool, job_id).await;
    assert_eq!(state, "finalized");
    assert_eq!(reason.as_deref(), Some("user_canceled"));

    // Idempotent: a second cancellation of the now-finalized job is a no-op.
    let again = client
        .delete(format!("http://{addr}/api/v1/jobs/{job_id}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), reqwest::StatusCode::NO_CONTENT);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn canceling_without_stop_permission_is_forbidden(pool: PgPool) {
    let addr = spawn_server(streaming_enabled_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    // `bob` owns the job; `carol` holds no permission on it.
    let bob_token = mock_login_token(&client, addr, "bob").await;
    let bob = whoami(&client, addr, &bob_token).await;
    let token_id = latest_token_id(&pool, bob).await;
    let job_id = seed_job(&pool, bob, token_id, &[]).await;

    let carol_token = mock_login_token(&client, addr, "carol").await;
    let resp = client
        .delete(format!("http://{addr}/api/v1/jobs/{job_id}"))
        .bearer_auth(&carol_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);

    // The job is untouched.
    let (state, _) = job_state_and_reason(&pool, job_id).await;
    assert_eq!(state, "queued");
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn canceling_a_nonexistent_job_is_forbidden(pool: PgPool) {
    let addr = spawn_server(streaming_enabled_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    let token = mock_login_token(&client, addr, "bob").await;
    let resp = client
        .delete(format!("http://{addr}/api/v1/jobs/{}", Uuid::new_v4()))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn owner_reads_own_job_with_secret_redacted(pool: PgPool) {
    let addr = spawn_server(streaming_enabled_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    // `bob` owns the job; his login also mints the token it is enqueued under.
    let token = mock_login_token(&client, addr, "bob").await;
    let bob = whoami(&client, addr, &token).await;
    let token_id = latest_token_id(&pool, bob).await;
    let job_id = seed_job(
        &pool,
        bob,
        token_id,
        &[("api_key", "s3cr3t", true), ("region", "us-east", false)],
    )
    .await;
    // The image the seed helper registered and bound onto the job.
    let image_id: Uuid =
        sqlx::query_scalar("select image_id from tml_switchboard.jobs where job_id = $1")
            .bind(job_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    let resp = client
        .get(format!("http://{addr}/api/v1/jobs/{job_id}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let info: JobInfo = resp.json().await.unwrap();
    assert_eq!(info.job_id, job_id);
    assert_eq!(info.owner_id, Some(bob));
    assert_eq!(info.state, JobState::Queued);
    assert!(matches!(info.image, JobImageRef::Image { image_id: got } if got == image_id));

    // Secret parameter: flagged secret, value withheld.
    let secret = &info.parameters["api_key"];
    assert!(secret.secret);
    assert_eq!(secret.value, None);
    // Non-secret parameter: value in the clear.
    let plain = &info.parameters["region"];
    assert!(!plain.secret);
    assert_eq!(plain.value.as_deref(), Some("us-east"));
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn admin_reads_any_job(pool: PgPool) {
    let addr = spawn_server(streaming_enabled_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    // `bob` owns the job; `alice` (a global admin) reads it.
    let bob_token = mock_login_token(&client, addr, "bob").await;
    let bob = whoami(&client, addr, &bob_token).await;
    let token_id = latest_token_id(&pool, bob).await;
    let job_id = seed_job(&pool, bob, token_id, &[]).await;

    let alice_token = mock_login_token(&client, addr, "alice").await;
    let resp = client
        .get(format!("http://{addr}/api/v1/jobs/{job_id}"))
        .bearer_auth(&alice_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(resp.json::<JobInfo>().await.unwrap().job_id, job_id);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn stranger_is_forbidden_from_reading_a_job(pool: PgPool) {
    let addr = spawn_server(streaming_enabled_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    // `bob` owns the job; `carol` (a plain user with no grant) is refused.
    let bob_token = mock_login_token(&client, addr, "bob").await;
    let bob = whoami(&client, addr, &bob_token).await;
    let token_id = latest_token_id(&pool, bob).await;
    let job_id = seed_job(&pool, bob, token_id, &[]).await;

    let carol_token = mock_login_token(&client, addr, "carol").await;
    let resp = client
        .get(format!("http://{addr}/api/v1/jobs/{job_id}"))
        .bearer_auth(&carol_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn reading_a_nonexistent_job_is_forbidden(pool: PgPool) {
    let addr = spawn_server(streaming_enabled_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    // A plain user gets 403 (not 404) for a job that does not exist: existence
    // is not leaked.
    let token = mock_login_token(&client, addr, "bob").await;
    let resp = client
        .get(format!("http://{addr}/api/v1/jobs/{}", Uuid::new_v4()))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn admin_gets_a_subscribe_token_for_any_job(pool: PgPool) {
    let addr = spawn_server(streaming_enabled_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    // `alice` is a global admin, so she can read any job — including one that
    // does not exist as a row, which is fine: the token is job-scoped by id.
    let token = mock_login_token(&client, addr, "alice").await;
    let job_id = Uuid::new_v4();

    let resp = client
        .post(format!("http://{addr}/api/v1/jobs/{job_id}/log-token"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let creds: LogStreamCredentials = resp.json().await.unwrap();
    assert_eq!(creds.nats_url, "nats://nats.example:4222");
    assert_eq!(creds.subject, format!("logs.{job_id}.>"));
    assert_eq!(creds.expires_in_secs, 300);
    assert!(!creds.token.is_empty(), "a token must be issued");
    // Sanity: a JWT has three dot-separated segments. (The token's scope/shape
    // is asserted exhaustively in the log_streaming unit tests.)
    assert_eq!(creds.token.split('.').count(), 3, "token looks like a JWT");
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn non_reader_is_forbidden(pool: PgPool) {
    let addr = spawn_server(streaming_enabled_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    // `bob` is a plain user with no grant on (and no ownership of) the job.
    let token = mock_login_token(&client, addr, "bob").await;
    let job_id = Uuid::new_v4();

    let resp = client
        .post(format!("http://{addr}/api/v1/jobs/{job_id}/log-token"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn streaming_disabled_yields_service_unavailable(pool: PgPool) {
    // `AppState::new` leaves log streaming unconfigured (None).
    let addr = spawn_server(AppState::new(pool.clone(), test_config_mock())).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();

    // Use the admin so authorization passes and we reach the streaming check.
    let token = mock_login_token(&client, addr, "alice").await;
    let job_id = Uuid::new_v4();

    let resp = client
        .post(format!("http://{addr}/api/v1/jobs/{job_id}/log-token"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
}

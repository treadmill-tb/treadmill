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
use treadmill_rs::api::switchboard::hosts::{
    HostCreateResponse, HostInfo, HostListEntry, HostRequirementsReport, HostSpecRejection,
    HostSpecUpdateResponse,
};
use treadmill_rs::host_spec::{HostSpec, PlatformKind};
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
        None,
        EventBus::default(),
    )
}

/// Insert a live host (heartbeat now) and describe it, so the listing has a
/// spec to return. Uses the runtime query API, so no `.sqlx` entry is needed.
async fn seed_live_host(pool: &PgPool, name: &str, owner: Uuid) -> Uuid {
    let host_id = Uuid::new_v4();
    // A unique 32-byte auth token (the column is `unique`); the first bytes
    // encode the host id so concurrent seeds in one test never collide.
    let mut auth_token = vec![0u8; 32];
    auth_token[..16].copy_from_slice(host_id.as_bytes());

    sqlx::query(
        "insert into tml_switchboard.hosts \
           (host_id, name, auth_token, last_seen_at, owner_id) \
         values ($1, $2, $3, now(), $4)",
    )
    .bind(host_id)
    .bind(name)
    .bind(auth_token)
    .bind(owner)
    .execute(pool)
    .await
    .unwrap();

    seed_spec(pool, host_id, name).await;
    host_id
}

/// Write revision 1 of a host's spec, with one DUT so the listing exercises a
/// non-trivial document.
async fn seed_spec(pool: &PgPool, host_id: Uuid, name: &str) {
    let spec = serde_json::json!({
        "spec_version": "v1",
        "id": host_id,
        "name": name,
        "description": "bring-up bench",
        "site": "cambridge",
        "location": null,
        "platform": {
            "kind": "physical", "arch": "aarch64",
            "profiles": ["rpi4-uboot-sd"],
            "vendor": "Raspberry Pi Ltd", "model": "Raspberry Pi 4 Model B"
        },
        "resources": { "cpu_cores": 4, "memory_mb": 8192, "storage_gb": 64 },
        "labels": { "bench": "nordic-bringup" },
        "duts": [{
            "name": "dut0", "serial": null, "vendor": "Nordic Semiconductor",
            "board": "nrf52840dk", "arch": ["cortex-m4"],
            "connectivity": ["ble"], "debug": null, "console": null, "labels": {}
        }]
    });
    sqlx::query(
        "insert into tml_switchboard.host_specs (host_id, revision, spec, spec_version) \
         values ($1, 1, $2, 'v1')",
    )
    .bind(host_id)
    .bind(&spec)
    .execute(pool)
    .await
    .unwrap();
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn lists_hosts_with_a_spec_projection(pool: PgPool) {
    let addr = spawn_server(test_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let token = mock_login_token(&pool, &client, addr, "bob", true).await;
    let owner = whoami(&client, addr, &token).await;
    let host_id = seed_live_host(&pool, "rpi-lab-03", owner).await;

    let resp = client
        .get(format!("http://{addr}/api/v1/hosts"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let hosts: Vec<HostListEntry> = resp.json().await.unwrap();
    let host = hosts
        .iter()
        .find(|h| h.host_id == host_id)
        .expect("seeded host present in listing");

    assert_eq!(host.name, "rpi-lab-03");
    assert!(host.live, "a host with a fresh heartbeat is live");
    assert!(host.last_seen_at.is_some());
    assert!(!host.maintenance);
    assert_eq!(host.spec_revision, Some(1));

    // What a fleet view is scanned by: the flat fields, and the identity of
    // what is attached.
    let spec = host.spec.clone().expect("host has a spec");
    assert_eq!(spec.site, "cambridge");
    assert_eq!(spec.description.as_deref(), Some("bring-up bench"));
    assert_eq!(spec.platform.kind, PlatformKind::Physical);
    assert_eq!(spec.platform.arch, "aarch64");
    assert_eq!(spec.platform.profiles, ["rpi4-uboot-sd"]);
    assert_eq!(spec.resources.memory_mb, 8192);
    assert_eq!(spec.duts.len(), 1);
    assert_eq!(spec.duts[0].board, "nrf52840dk");
    assert_eq!(spec.duts[0].vendor, "Nordic Semiconductor");
    assert_eq!(
        spec.labels.get("bench").map(String::as_str),
        Some("nordic-bringup")
    );

    // The rest of the document is the detail view's, so a listing does not grow
    // with what is wired to each host.
    let listed = serde_json::to_value(host).unwrap();
    for dropped in ["serial", "connectivity", "debug", "console", "model"] {
        assert!(
            !listed.to_string().contains(dropped),
            "the listing must not carry `{dropped}`"
        );
    }
}

/// A spec is visible to anyone who can read its host, so the listing is scoped
/// to `read` rather than showing the whole fleet.
#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn listing_is_scoped_to_readable_hosts(pool: PgPool) {
    let addr = spawn_server(test_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let bob_token = mock_login_token(&pool, &client, addr, "bob", true).await;
    let bob = whoami(&client, addr, &bob_token).await;
    let bobs_host = seed_live_host(&pool, "bobs-host", bob).await;

    let carol_token = mock_login_token(&pool, &client, addr, "carol", true).await;
    let carol = whoami(&client, addr, &carol_token).await;
    let carols_host = seed_live_host(&pool, "carols-host", carol).await;

    let listing = |token: String| {
        let client = client.clone();
        async move {
            client
                .get(format!("http://{addr}/api/v1/hosts"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .json::<Vec<HostListEntry>>()
                .await
                .unwrap()
                .into_iter()
                .map(|h| h.host_id)
                .collect::<Vec<_>>()
        }
    };

    assert_eq!(listing(bob_token.clone()).await, vec![bobs_host]);
    assert_eq!(listing(carol_token).await, vec![carols_host]);

    // A `read` grant brings carol's host into bob's listing.
    sqlx::query(
        "insert into tml_switchboard.host_grants (host_id, subject_id, permission) \
         values ($1, $2, 'read')",
    )
    .bind(carols_host)
    .bind(bob)
    .execute(&pool)
    .await
    .unwrap();
    let mut seen = listing(bob_token).await;
    seen.sort();
    let mut expected = vec![bobs_host, carols_host];
    expected.sort();
    assert_eq!(seen, expected);
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
           (host_id, name, auth_token, worker_instance_id, owner_id) \
         values ($1, $2, $3, 0, $4)",
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

/// `PATCH /hosts/{id}` requires `manage`, which the host's owner holds
/// implicitly; a non-owner is refused and the flag is untouched.
#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn patch_host_maintenance(pool: PgPool) {
    let addr = spawn_server(test_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let owner_token = mock_login_token(&pool, &client, addr, "bob", true).await;
    let owner = whoami(&client, addr, &owner_token).await;
    let host_id = seed_host_owned(&pool, "cam-rpi4-01", owner).await;

    let patch = |token: String, body: serde_json::Value| {
        let client = client.clone();
        async move {
            client
                .patch(format!("http://{addr}/api/v1/hosts/{host_id}"))
                .bearer_auth(token)
                .json(&body)
                .send()
                .await
                .unwrap()
                .status()
        }
    };
    let maintenance = || async {
        sqlx::query_scalar::<_, bool>(
            "select maintenance from tml_switchboard.hosts where host_id = $1",
        )
        .bind(host_id)
        .fetch_one(&pool)
        .await
        .unwrap()
    };

    assert!(!maintenance().await);

    let stranger_token = mock_login_token(&pool, &client, addr, "carol", true).await;
    assert_eq!(
        patch(stranger_token, serde_json::json!({ "maintenance": true })).await,
        reqwest::StatusCode::FORBIDDEN
    );
    assert!(!maintenance().await, "a refused request changes nothing");

    assert_eq!(
        patch(
            owner_token.clone(),
            serde_json::json!({ "maintenance": true })
        )
        .await,
        reqwest::StatusCode::NO_CONTENT
    );
    assert!(maintenance().await);

    // An empty patch, and one already in force, are both no-ops.
    assert_eq!(
        patch(owner_token.clone(), serde_json::json!({})).await,
        reqwest::StatusCode::NO_CONTENT
    );
    assert_eq!(
        patch(
            owner_token.clone(),
            serde_json::json!({ "maintenance": true })
        )
        .await,
        reqwest::StatusCode::NO_CONTENT
    );
    assert!(maintenance().await);

    assert_eq!(
        patch(owner_token, serde_json::json!({ "maintenance": false })).await,
        reqwest::StatusCode::NO_CONTENT
    );
    assert!(!maintenance().await);
}

/// The listing reports the flag, so an operator can see which hosts are held
/// out of service.
#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn listing_reports_maintenance(pool: PgPool) {
    let addr = spawn_server(test_state(pool.clone())).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let token = mock_login_token(&pool, &client, addr, "bob", true).await;
    let owner = whoami(&client, addr, &token).await;
    let host_id = seed_live_host(&pool, "cam-rpi4-01", owner).await;
    sqlx::query("update tml_switchboard.hosts set maintenance = true where host_id = $1")
        .bind(host_id)
        .execute(&pool)
        .await
        .unwrap();
    let hosts: Vec<HostListEntry> = client
        .get(format!("http://{addr}/api/v1/hosts"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let host = hosts.iter().find(|h| h.host_id == host_id).unwrap();
    assert!(host.maintenance);
}

// -- host creation and spec writes ----------------------------------------

/// A valid v1 spec document for `host_id`, as an admin would hand-write it.
fn spec_document(host_id: Uuid, name: &str) -> serde_json::Value {
    serde_json::json!({
        "spec_version": "v1",
        "id": host_id,
        "name": name,
        "description": null,
        "site": "cambridge",
        "location": null,
        "platform": {
            "kind": "virtual", "arch": "x86_64",
            "profiles": ["q35-virtio-uefi"], "hypervisor": "qemu"
        },
        "resources": { "cpu_cores": 8, "memory_mb": 16384, "storage_gb": 200 },
        "labels": {},
        "duts": []
    })
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// `POST /hosts` writes the row and revision 1 together, so a host is never in
/// an undescribed state, and hands back the supervisor credential once.
#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn create_host_writes_the_row_and_its_first_spec(pool: PgPool) {
    use base64::Engine as _;

    let addr = spawn_server(test_state(pool.clone())).await;
    let client = client();
    let admin = mock_login_token(&pool, &client, addr, "alice", true).await;

    let host_id = Uuid::new_v4();
    let resp = client
        .post(format!("http://{addr}/api/v1/hosts"))
        .bearer_auth(&admin)
        .json(&serde_json::json!({ "spec": spec_document(host_id, "cam-qemu-04") }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let created: HostCreateResponse = resp.json().await.unwrap();
    assert_eq!(created.host_id, host_id);
    assert_eq!(created.spec_revision, 1);

    // The returned credential is the one the supervisor will present.
    let stored: Vec<u8> =
        sqlx::query_scalar("select auth_token from tml_switchboard.hosts where host_id = $1")
            .bind(host_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let presented = base64::engine::general_purpose::STANDARD
        .decode(&created.auth_token)
        .expect("the token is base64");
    assert_eq!(presented, stored);

    // The spec comes back through the single-host route.
    let host: HostInfo = client
        .get(format!("http://{addr}/api/v1/hosts/{host_id}"))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(host.spec_revision, Some(1));
    let HostSpec::V1(spec) = host.spec.expect("the host is described");
    assert_eq!(spec.name, "cam-qemu-04");
    assert_eq!(spec.platform.profiles(), ["q35-virtio-uefi"]);

    // The id is the client's to choose, so reusing it is a conflict.
    let again = client
        .post(format!("http://{addr}/api/v1/hosts"))
        .bearer_auth(&admin)
        .json(&serde_json::json!({ "spec": spec_document(host_id, "cam-qemu-04") }))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), reqwest::StatusCode::CONFLICT);
}

/// Creating a host mints a supervisor credential and puts a machine into
/// scheduling, so it is global-admin only.
#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn create_host_requires_a_global_admin(pool: PgPool) {
    let addr = spawn_server(test_state(pool.clone())).await;
    let client = client();
    let bob = mock_login_token(&pool, &client, addr, "bob", true).await;

    let resp = client
        .post(format!("http://{addr}/api/v1/hosts"))
        .bearer_auth(&bob)
        .json(&serde_json::json!({ "spec": spec_document(Uuid::new_v4(), "nope") }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

/// A spec is hand-edited, so a rejection names the offending field rather than
/// a byte offset.
#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn create_host_rejects_a_bad_spec_with_a_field_path(pool: PgPool) {
    let addr = spawn_server(test_state(pool.clone())).await;
    let client = client();
    let admin = mock_login_token(&pool, &client, addr, "alice", true).await;

    let post = async |spec: serde_json::Value| {
        let resp = client
            .post(format!("http://{addr}/api/v1/hosts"))
            .bearer_auth(&admin)
            .json(&serde_json::json!({ "spec": spec }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        resp.json::<HostSpecRejection>().await.unwrap()
    };

    // A typo deep inside a DUT is reported at its own path, which is the whole
    // reason validation does not go through the untagged `HostSpec`.
    let mut spec = spec_document(Uuid::new_v4(), "cam-qemu-04");
    spec["duts"] = serde_json::json!([{
        "name": null, "serial": null, "vendor": "SEGGER", "board": "nrf52840dk",
        "arch": [], "connectivity": [], "console": null, "labels": {},
        "debug": { "protocol": "swd", "probe": {
            "vendor": "SEGGER", "model": "J-Link OB", "serail": "000683012345"
        } }
    }]);
    let rejection = post(spec).await;
    assert_eq!(rejection.path, "duts[0].debug.probe.serail");
    assert!(rejection.message.contains("unknown field"), "{rejection:?}");

    // An unknown version is refused at the discriminant, not the root.
    let mut spec = spec_document(Uuid::new_v4(), "cam-qemu-04");
    spec["spec_version"] = "v99".into();
    assert_eq!(post(spec).await.path, "spec_version");
}

/// Spec writes are append-only: a write adds a revision, the newest is the one
/// in force, and the one it replaced is still in the history.
#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn put_spec_appends_a_revision(pool: PgPool) {
    let addr = spawn_server(test_state(pool.clone())).await;
    let client = client();
    let admin = mock_login_token(&pool, &client, addr, "alice", true).await;

    let host_id = Uuid::new_v4();
    client
        .post(format!("http://{addr}/api/v1/hosts"))
        .bearer_auth(&admin)
        .json(&serde_json::json!({ "spec": spec_document(host_id, "cam-qemu-04") }))
        .send()
        .await
        .unwrap();

    let put = async |spec: serde_json::Value| {
        client
            .put(format!("http://{addr}/api/v1/hosts/{host_id}/spec"))
            .bearer_auth(&admin)
            .json(&serde_json::json!({ "spec": spec }))
            .send()
            .await
            .unwrap()
    };

    let mut edited = spec_document(host_id, "cam-qemu-04");
    edited["site"] = "oxford".into();
    let resp = put(edited.clone()).await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(
        resp.json::<HostSpecUpdateResponse>()
            .await
            .unwrap()
            .spec_revision,
        2
    );

    // The read reflects the newest revision, and the history is intact.
    let host: HostInfo = client
        .get(format!("http://{addr}/api/v1/hosts/{host_id}"))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(host.spec_revision, Some(2));
    let HostSpec::V1(spec) = host.spec.expect("the host is described");
    assert_eq!(spec.site, "oxford");

    let revisions: Vec<i32> = sqlx::query_scalar(
        "select revision from tml_switchboard.host_specs where host_id = $1 order by revision",
    )
    .bind(host_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(revisions, vec![1, 2], "revision 1 is kept, not replaced");
}

/// A spec names the host it describes, and the write route says so with a
/// field path rather than letting the table CHECK surface as a 500.
#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn put_spec_rejects_a_document_for_another_host(pool: PgPool) {
    let addr = spawn_server(test_state(pool.clone())).await;
    let client = client();
    let admin = mock_login_token(&pool, &client, addr, "alice", true).await;

    let host_id = Uuid::new_v4();
    client
        .post(format!("http://{addr}/api/v1/hosts"))
        .bearer_auth(&admin)
        .json(&serde_json::json!({ "spec": spec_document(host_id, "cam-qemu-04") }))
        .send()
        .await
        .unwrap();

    let resp = client
        .put(format!("http://{addr}/api/v1/hosts/{host_id}/spec"))
        .bearer_auth(&admin)
        .header("if-match", "1")
        .json(&serde_json::json!({ "spec": spec_document(Uuid::new_v4(), "someone-else") }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(resp.json::<HostSpecRejection>().await.unwrap().path, "id");
}

/// Describing a host is a `manage` operation, like its operational state; a
/// plain reader cannot write one.
#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn put_spec_requires_manage(pool: PgPool) {
    let addr = spawn_server(test_state(pool.clone())).await;
    let client = client();
    let admin = mock_login_token(&pool, &client, addr, "alice", true).await;
    let bob = mock_login_token(&pool, &client, addr, "bob", true).await;
    let bob_id = whoami(&client, addr, &bob).await;

    let host_id = Uuid::new_v4();
    client
        .post(format!("http://{addr}/api/v1/hosts"))
        .bearer_auth(&admin)
        .json(&serde_json::json!({ "spec": spec_document(host_id, "cam-qemu-04") }))
        .send()
        .await
        .unwrap();
    // `read` is enough to see the spec, and deliberately not enough to write it.
    sqlx::query(
        "insert into tml_switchboard.host_grants (host_id, subject_id, permission) \
         values ($1, $2, 'read')",
    )
    .bind(host_id)
    .bind(bob_id)
    .execute(&pool)
    .await
    .unwrap();

    let resp = client
        .put(format!("http://{addr}/api/v1/hosts/{host_id}/spec"))
        .bearer_auth(&bob)
        .header("if-match", "1")
        .json(&serde_json::json!({ "spec": spec_document(host_id, "cam-qemu-04") }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);

    // ... but reading it is fine.
    let read = client
        .get(format!("http://{addr}/api/v1/hosts/{host_id}"))
        .bearer_auth(&bob)
        .send()
        .await
        .unwrap();
    assert_eq!(read.status(), reqwest::StatusCode::OK);
}

/// `GET /hosts/{id}` is scoped to `read`, and an unreadable host is refused
/// rather than reported missing, so the route does not leak which ids exist.
#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn get_host_is_scoped_to_read(pool: PgPool) {
    let addr = spawn_server(test_state(pool.clone())).await;
    let client = client();
    let admin = mock_login_token(&pool, &client, addr, "alice", true).await;
    let bob = mock_login_token(&pool, &client, addr, "bob", true).await;

    let host_id = Uuid::new_v4();
    client
        .post(format!("http://{addr}/api/v1/hosts"))
        .bearer_auth(&admin)
        .json(&serde_json::json!({ "spec": spec_document(host_id, "cam-qemu-04") }))
        .send()
        .await
        .unwrap();

    for (token, expected) in [
        (&bob, reqwest::StatusCode::FORBIDDEN),
        (&admin, reqwest::StatusCode::OK),
    ] {
        let resp = client
            .get(format!("http://{addr}/api/v1/hosts/{host_id}"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), expected);
    }

    // A host that does not exist is the same 403, for the same reason.
    let resp = client
        .get(format!("http://{addr}/api/v1/hosts/{}", Uuid::new_v4()))
        .bearer_auth(&bob)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

// -- the host-matching diagnostic -----------------------------------------

/// Create a host through the API and return its id.
async fn create_host(
    client: &reqwest::Client,
    addr: SocketAddr,
    admin: &str,
    spec: serde_json::Value,
) -> Uuid {
    let resp = client
        .post(format!("http://{addr}/api/v1/hosts"))
        .bearer_auth(admin)
        .json(&serde_json::json!({ "spec": spec }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    resp.json::<HostCreateResponse>().await.unwrap().host_id
}

/// A spec advertising `profile`, with `memory_mb` of RAM.
fn spec_with(host_id: Uuid, name: &str, profile: &str, memory_mb: u32) -> serde_json::Value {
    let mut spec = spec_document(host_id, name);
    spec["platform"]["profiles"] = serde_json::json!([profile]);
    spec["resources"]["memory_mb"] = memory_mb.into();
    spec
}

async fn validate(
    client: &reqwest::Client,
    addr: SocketAddr,
    token: &str,
    body: serde_json::Value,
) -> HostRequirementsReport {
    let resp = client
        .post(format!("http://{addr}/api/v1/hosts/match"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    resp.json().await.unwrap()
}

/// The served schema is the committed snapshot, not a second copy of it: the
/// console renders from this, and a drifting copy would mislead an author
/// about what the switchboard actually accepts.
#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn spec_schema_is_the_committed_artifact(pool: PgPool) {
    let addr = spawn_server(test_state(pool.clone())).await;
    let client = client();
    let token = mock_login_token(&pool, &client, addr, "bob", true).await;

    let served: serde_json::Value = client
        .get(format!("http://{addr}/api/v1/hosts/spec-schema"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let snapshot = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../treadmill-rs/protocol-schema/host_spec.schema.json"
    ))
    .expect("the committed snapshot exists");
    let committed: serde_json::Value = serde_json::from_str(&snapshot).unwrap();
    assert_eq!(served, committed);
}

/// The counts answer the question a queued job cannot: is this predicate ever
/// going to be placed?
#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn validate_counts_predicate_matches(pool: PgPool) {
    let addr = spawn_server(test_state(pool.clone())).await;
    let client = client();
    let admin = mock_login_token(&pool, &client, addr, "alice", true).await;

    create_host(
        &client,
        addr,
        &admin,
        spec_with(Uuid::new_v4(), "small", "q35-virtio-uefi", 4096),
    )
    .await;
    let big = create_host(
        &client,
        addr,
        &admin,
        spec_with(Uuid::new_v4(), "big", "q35-virtio-uefi", 16384),
    )
    .await;

    let report = validate(
        &client,
        addr,
        &admin,
        serde_json::json!({ "host_cel_predicate": "host.resources.memory_mb >= 16384" }),
    )
    .await;
    assert_eq!(report.authorized, 2);
    assert_eq!(report.predicate_matched, 1);
    // Named, not counted: the report says *which* host a query selected.
    assert_eq!(report.schedulable, vec![big]);
    // No image set named, so nothing to report on the image side.
    assert_eq!(report.image_matched, None);
    assert_eq!(report.errored, 0);
    assert_eq!(report.compile_error, None);

    // A predicate nothing satisfies is reported as such, not as an error.
    let report = validate(
        &client,
        addr,
        &admin,
        serde_json::json!({ "host_cel_predicate": "host.site == 'atlantis'" }),
    )
    .await;
    assert_eq!(report.authorized, 2);
    assert_eq!(report.predicate_matched, 0);
    assert_eq!(report.errored, 0);
}

/// A predicate that does not compile is reported as such: nothing is
/// evaluated, so an empty match count would be misleading.
#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn validate_reports_a_compile_error(pool: PgPool) {
    let addr = spawn_server(test_state(pool.clone())).await;
    let client = client();
    let admin = mock_login_token(&pool, &client, addr, "alice", true).await;
    create_host(
        &client,
        addr,
        &admin,
        spec_with(Uuid::new_v4(), "one", "q35-virtio-uefi", 4096),
    )
    .await;

    let report = validate(
        &client,
        addr,
        &admin,
        serde_json::json!({ "host_cel_predicate": "host.site ==" }),
    )
    .await;
    assert!(report.compile_error.is_some(), "{report:?}");
    assert_eq!(report.authorized, 1, "the fleet is still counted");
    assert_eq!(report.predicate_matched, 0);
    assert!(report.schedulable.is_empty());
}

/// An unguarded reach into a variant's field errors per host. Those are
/// surfaced rather than folded into the miss count: a forgotten `has()` guard
/// otherwise looks exactly like an empty fleet.
#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn validate_surfaces_evaluation_errors(pool: PgPool) {
    let addr = spawn_server(test_state(pool.clone())).await;
    let client = client();
    let admin = mock_login_token(&pool, &client, addr, "alice", true).await;
    let host_id = create_host(
        &client,
        addr,
        &admin,
        spec_with(Uuid::new_v4(), "cam-qemu-04", "q35-virtio-uefi", 4096),
    )
    .await;

    // `model` exists only on the physical variant; the seeded host is virtual.
    let report = validate(
        &client,
        addr,
        &admin,
        serde_json::json!({ "host_cel_predicate": "host.platform.model == 'rpi4'" }),
    )
    .await;
    assert_eq!(report.predicate_matched, 0);
    assert_eq!(report.errored, 1);
    assert_eq!(report.errors.len(), 1);
    assert_eq!(report.errors[0].host_id, host_id);
    assert_eq!(report.errors[0].name, "cam-qemu-04");
    assert!(report.errors[0].message.contains("model"), "{report:?}");

    // The guarded form is a clean non-match, not an error.
    let report = validate(
        &client,
        addr,
        &admin,
        serde_json::json!({
            "host_cel_predicate": "has(host.platform.model) && host.platform.model == 'rpi4'"
        }),
    )
    .await;
    assert_eq!(report.errored, 0);
    assert_eq!(report.predicate_matched, 0);
}

/// The diagnostic deliberately does not short-circuit the way the scheduler
/// does: both filters run over the whole authorized set, so "your query
/// matches nothing" stays distinguishable from "your image has no member for
/// the hosts it matched". From a queued job the two look identical.
#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn validate_separates_predicate_misses_from_image_misses(pool: PgPool) {
    use treadmill_rs::image::{Digest, media_types};
    use treadmill_switchboard::sql;

    let addr = spawn_server(test_state(pool.clone())).await;
    let client = client();
    let admin = mock_login_token(&pool, &client, addr, "alice", true).await;
    let admin_id = whoami(&client, addr, &admin).await;

    // Two hosts, both `q35-virtio-uefi`, differing only in memory.
    create_host(
        &client,
        addr,
        &admin,
        spec_with(Uuid::new_v4(), "small", "q35-virtio-uefi", 4096),
    )
    .await;
    create_host(
        &client,
        addr,
        &admin,
        spec_with(Uuid::new_v4(), "big", "q35-virtio-uefi", 16384),
    )
    .await;

    // A set whose only member is built for a profile neither host advertises.
    let set_id = Uuid::new_v4();
    let image_id = Uuid::new_v4();
    let digest = Digest::from_sha256([7u8; 32]);
    let mut txn = pool.begin().await.unwrap();
    sql::image::create_set(&mut *txn, set_id, "rpi-only", admin_id, None)
        .await
        .unwrap();
    sql::image::insert(
        &mut *txn,
        image_id,
        &digest.encoded(),
        media_types::IMAGE_ARTIFACT_TYPE,
        None,
    )
    .await
    .unwrap();
    sql::image::insert_source(
        &mut *txn,
        Uuid::new_v4(),
        image_id,
        "reg.example:5000",
        "repo",
        "external",
        Some(admin_id),
    )
    .await
    .unwrap();
    sql::image::create_generation(
        &mut txn,
        set_id,
        admin_id,
        &[sql::image::NewSetMember {
            image_id,
            platform_profile: "rpi4-uboot-sd".to_string(),
            predicate: None,
            index: 0,
        }],
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();

    let body = |predicate: &str| {
        serde_json::json!({
            "host_cel_predicate": predicate,
            "init_spec": { "type": "image_set", "set_id": set_id, "generation": null }
        })
    };

    // The predicate matches everything; the image matches nothing. Without the
    // split this would read as a query problem.
    let report = validate(&client, addr, &admin, body("true")).await;
    assert_eq!(report.authorized, 2);
    assert_eq!(report.predicate_matched, 2, "the predicate is fine");
    assert_eq!(
        report.image_matched,
        Some(0),
        "the image set is the problem"
    );
    assert!(report.schedulable.is_empty());

    // The mirror image: the predicate is the problem, and the image side is
    // still reported over the whole set rather than over what survived.
    let report = validate(&client, addr, &admin, body("host.site == 'atlantis'")).await;
    assert_eq!(report.predicate_matched, 0);
    assert_eq!(report.image_matched, Some(0));
    assert!(report.schedulable.is_empty());
}

/// Counts cover only hosts the caller may start on, so the endpoint cannot be
/// used to probe the existence or properties of hosts it cannot reach.
#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn validate_counts_only_authorized_hosts(pool: PgPool) {
    let addr = spawn_server(test_state(pool.clone())).await;
    let client = client();
    let admin = mock_login_token(&pool, &client, addr, "alice", true).await;
    let bob = mock_login_token(&pool, &client, addr, "bob", true).await;
    let bob_id = whoami(&client, addr, &bob).await;

    let host_id = create_host(
        &client,
        addr,
        &admin,
        spec_with(Uuid::new_v4(), "cam-qemu-04", "q35-virtio-uefi", 16384),
    )
    .await;

    let query = serde_json::json!({ "host_cel_predicate": "true" });
    // Admin sees the fleet; bob has no grant on anything.
    assert_eq!(
        validate(&client, addr, &admin, query.clone())
            .await
            .authorized,
        1
    );
    let report = validate(&client, addr, &bob, query.clone()).await;
    assert_eq!(report.authorized, 0);
    assert_eq!(report.predicate_matched, 0);

    // `read` is not `start`, so it does not bring the host into bob's counts.
    for permission in ["read", "start"] {
        sqlx::query(
            "insert into tml_switchboard.host_grants (host_id, subject_id, permission) \
             values ($1, $2, $3::tml_switchboard.host_permission)",
        )
        .bind(host_id)
        .bind(bob_id)
        .bind(permission)
        .execute(&pool)
        .await
        .unwrap();
        let expected = u32::from(permission == "start");
        assert_eq!(
            validate(&client, addr, &bob, query.clone())
                .await
                .authorized,
            expected,
            "after granting {permission}"
        );
    }
}

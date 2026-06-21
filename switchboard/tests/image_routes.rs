//! End-to-end tests for the image-catalog REST API.
//!
//! Drive the real router over a loopback socket against ephemeral Postgres,
//! authenticating via the development mock-OAuth provider (no external service,
//! so multiple distinct users — `bob`, `carol` — are trivial to provision). The
//! registry the `/images` routes pull manifests from is a canned in-memory
//! [`StubRegistry`] (these tests have no Zot), so validation runs against real
//! OCI wire-format JSON without a registry daemon.
//!
//! Image *groups* are mutable, named switchboard entities: created empty, then
//! given membership by appending immutable, full-replacement generations whose
//! members are pre-registered images referenced by id. No registry pull is
//! involved in group/generation creation.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::redirect::Policy;
use sha2::{Digest as _, Sha256};
use sqlx::PgPool;
use tokio::net::TcpListener;
use uuid::Uuid;

use treadmill_rs::api::switchboard::images::{ImageGroupGenerationInfo, ImageGroupInfo, ImageInfo};
use treadmill_rs::api::switchboard::jobs::{EnqueueJobResponse, JobImageRef, JobInfo};
use treadmill_rs::api::switchboard::{JobInitSpec, JobRequest, LoginResponse, WhoAmIResponse};
use treadmill_rs::api::switchboard_supervisor::RestartPolicy;
use treadmill_rs::image::Digest;
use treadmill_switchboard::registry::{RegistryClient, RegistryError};
use treadmill_switchboard::routes::build_router;
use treadmill_switchboard::serve::AppState;

mod common;
use common::test_config_mock;

// -- canned registry ------------------------------------------------------------

/// An in-memory registry mapping `(registry, repository, digest)` to the exact
/// manifest bytes the test staged.
#[derive(Default)]
struct StubRegistry {
    docs: HashMap<(String, String, String), Vec<u8>>,
}

impl StubRegistry {
    /// Stage a document; returns its content digest (which the routes verify).
    fn put(&mut self, registry: &str, repository: &str, bytes: Vec<u8>) -> Digest {
        let digest = sha256(&bytes);
        self.docs.insert(
            (
                registry.to_string(),
                repository.to_string(),
                digest.encoded(),
            ),
            bytes,
        );
        digest
    }
}

#[async_trait]
impl RegistryClient for StubRegistry {
    async fn fetch_manifest(
        &self,
        registry: &str,
        repository: &str,
        digest: &Digest,
    ) -> Result<Vec<u8>, RegistryError> {
        self.docs
            .get(&(
                registry.to_string(),
                repository.to_string(),
                digest.encoded(),
            ))
            .cloned()
            .ok_or_else(|| RegistryError::Pull {
                reference: format!("{registry}/{repository}@{digest}"),
                source: oci_client::errors::OciDistributionError::ImageManifestNotFoundError(
                    digest.encoded(),
                ),
            })
    }
}

fn sha256(bytes: &[u8]) -> Digest {
    let hash = Sha256::digest(bytes);
    Digest::from_sha256(hash.into())
}

/// A valid two-layer Treadmill image manifest, parameterized so distinct images
/// hash to distinct digests via the title.
fn image_manifest_bytes(title: &str) -> Vec<u8> {
    const BASE: &str = "sha256:e839ce3984083b7c9b491615aa0382d159c5ee0204d252cce5efcf0225f1a622";
    const OVERLAY: &str = "sha256:3286aac796e2fcf217eb5a2f9430022aa16a6f1247182b35170b71cb196c6fe8";
    const EMPTY: &str = "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";
    format!(
        r#"{{
          "schemaVersion": 2,
          "mediaType": "application/vnd.oci.image.manifest.v1+json",
          "artifactType": "application/vnd.treadmill.image.v1+json",
          "config": {{ "mediaType": "application/vnd.oci.empty.v1+json", "digest": "{EMPTY}", "size": 2 }},
          "layers": [
            {{ "mediaType": "application/vnd.treadmill.disk.qcow2", "digest": "{BASE}", "size": 2085355520,
               "annotations": {{ "ci.treadmill.role": "root", "ci.treadmill.qcow2.virtual-size": "2294284288" }} }},
            {{ "mediaType": "application/vnd.treadmill.disk.qcow2", "digest": "{OVERLAY}", "size": 3145728,
               "annotations": {{ "ci.treadmill.role": "root", "ci.treadmill.qcow2.virtual-size": "4294967296",
                                 "ci.treadmill.qcow2.lower": "{BASE}" }} }}
          ],
          "annotations": {{ "org.opencontainers.image.title": "{title}", "ci.treadmill.qcow2.head": "{OVERLAY}" }}
        }}"#
    )
    .into_bytes()
}

// -- harness --------------------------------------------------------------------

const REGISTRY: &str = "registry.example.com:5000";
const REPO: &str = "u/octocat/ubuntu";

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

/// Spawn the router backed by `registry` and the mock-OAuth config.
async fn spawn_with_registry(pool: &PgPool, registry: Arc<dyn RegistryClient>) -> SocketAddr {
    let state = AppState::with_registry(pool.clone(), test_config_mock(), registry);
    spawn_server(state).await
}

/// A reqwest client that does not auto-follow redirects (the mock login flow
/// hands back a `Location` we follow by hand).
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap()
}

/// Drive a full mock login for `identity` and return the issued bearer token.
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
    assert_eq!(cb.status(), reqwest::StatusCode::OK);
    cb.json::<LoginResponse>()
        .await
        .unwrap()
        .token
        .encode_for_http()
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

/// Register a staged manifest via `POST /images` and return its catalog view.
async fn register_image(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    digest: &Digest,
) -> ImageInfo {
    let resp = client
        .post(format!("{base}/images"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "registry": REGISTRY,
            "repository": REPO,
            "manifest_digest": digest.encoded(),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    resp.json().await.unwrap()
}

/// Create an empty image group via `POST /image-groups` and return its view.
async fn create_group(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    name: &str,
    public: bool,
) -> ImageGroupInfo {
    let resp = client
        .post(format!("{base}/image-groups"))
        .bearer_auth(token)
        .json(&serde_json::json!({ "name": name, "public": public }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    resp.json().await.unwrap()
}

/// Append a one-member generation referencing `image_id`.
async fn add_generation(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    group: Uuid,
    image: Uuid,
) {
    let resp = client
        .post(format!("{base}/image-groups/{group}/generations"))
        .bearer_auth(token)
        .json(&serde_json::json!({ "members": [
            { "image_id": image, "required_host_tags": [] },
        ] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
}

/// `POST /jobs` referencing an image group; returns the raw response so the
/// caller can assert on the status (the enqueue authorization is under test).
async fn enqueue_group(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    group: Uuid,
    generation: Option<u32>,
) -> reqwest::Response {
    let req = JobRequest {
        init_spec: JobInitSpec::ImageGroup {
            image_group: group,
            generation,
        },
        owner: None,
        ssh_keys: vec![],
        restart_policy: RestartPolicy {
            remaining_restart_count: 0,
        },
        parameters: HashMap::new(),
        host_tag_requirements: vec![],
        target_requirements: vec![],
        override_timeout: None,
    };
    client
        .post(format!("{base}/jobs"))
        .bearer_auth(token)
        .json(&req)
        .send()
        .await
        .unwrap()
}

// -- image registration ---------------------------------------------------------

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn register_list_and_inspect_image(pool: PgPool) {
    let mut reg = StubRegistry::default();
    let digest = reg.put(REGISTRY, REPO, image_manifest_bytes("Ubuntu test"));
    let addr = spawn_with_registry(&pool, Arc::new(reg)).await;
    let client = http_client();
    let token = mock_login_token(&client, addr, "bob").await;
    let base = format!("http://{addr}/api/v1");

    // POST /images registers the image and reports its single external location.
    let resp = client
        .post(format!("{base}/images"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "registry": REGISTRY,
            "repository": REPO,
            "manifest_digest": digest.encoded(),
            "label": "my ubuntu",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let info: ImageInfo = resp.json().await.unwrap();
    assert_eq!(info.manifest_digest, digest);
    assert_eq!(info.label.as_deref(), Some("my ubuntu"));
    assert_eq!(info.locations.len(), 1);
    assert_eq!(info.locations[0].registry, REGISTRY);
    assert_eq!(info.locations[0].status, "external");

    // Re-registering the same digest is idempotent (200, not a duplicate).
    let resp = client
        .post(format!("{base}/images"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "registry": REGISTRY, "repository": REPO, "manifest_digest": digest.encoded(),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    // GET /images lists exactly the one image.
    let list: Vec<ImageInfo> = client
        .get(format!("{base}/images"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].manifest_digest, digest);

    // GET /images/{digest} inspects it.
    let one: ImageInfo = client
        .get(format!("{base}/images/{}", digest.encoded()))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(one.id, info.id);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn rejects_unknown_and_malformed_manifests(pool: PgPool) {
    // An empty registry: nothing staged, so the pull fails (502).
    let addr = spawn_with_registry(&pool, Arc::new(StubRegistry::default())).await;
    let client = http_client();
    let token = mock_login_token(&client, addr, "bob").await;
    let base = format!("http://{addr}/api/v1");

    let unknown = Digest::from_sha256([7u8; 32]);
    let resp = client
        .post(format!("{base}/images"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "registry": REGISTRY, "repository": REPO, "manifest_digest": unknown.encoded(),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_GATEWAY);

    // A staged document that is not a Treadmill artifact is rejected (422).
    let mut reg = StubRegistry::default();
    let bogus = reg.put(REGISTRY, REPO, br#"{"schemaVersion":2,"mediaType":"x","config":{"mediaType":"y","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","size":0},"layers":[]}"#.to_vec());
    let addr = spawn_with_registry(&pool, Arc::new(reg)).await;
    let token = mock_login_token(&client, addr, "bob").await;
    let base = format!("http://{addr}/api/v1");
    let resp = client
        .post(format!("{base}/images"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "registry": REGISTRY, "repository": REPO, "manifest_digest": bogus.encoded(),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
}

// -- image groups ---------------------------------------------------------------

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn create_group_append_generations_and_inspect(pool: PgPool) {
    // Two member images, registered as concrete images first.
    let mut reg = StubRegistry::default();
    let m0 = reg.put(REGISTRY, REPO, image_manifest_bytes("member 0"));
    let m1 = reg.put(REGISTRY, REPO, image_manifest_bytes("member 1"));
    let addr = spawn_with_registry(&pool, Arc::new(reg)).await;
    let client = http_client();
    let token = mock_login_token(&client, addr, "bob").await;
    let base = format!("http://{addr}/api/v1");

    let img0 = register_image(&client, &base, &token, &m0).await;
    let img1 = register_image(&client, &base, &token, &m1).await;

    // POST /image-groups creates an empty, named group with no generation yet.
    let resp = client
        .post(format!("{base}/image-groups"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "name": "ubuntu", "label": "Ubuntu group" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let group: ImageGroupInfo = resp.json().await.unwrap();
    assert_eq!(group.name, "ubuntu");
    assert_eq!(group.label.as_deref(), Some("Ubuntu group"));
    assert_eq!(group.latest_generation, None);

    // POST a first generation: member 0 generic, member 1 more specific.
    let resp = client
        .post(format!("{base}/image-groups/{}/generations", group.id))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "members": [
            { "image_id": img0.id, "required_host_tags": ["arch=arm64"] },
            { "image_id": img1.id, "required_host_tags": ["arch=arm64", "raspberrypi-4"] },
        ] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let gen1: ImageGroupGenerationInfo = resp.json().await.unwrap();
    assert_eq!(gen1.generation, 1);
    assert_eq!(gen1.members.len(), 2);
    // Members come back in array (index) order; the manifest digest is recovered.
    assert_eq!(gen1.members[0].index, 0);
    assert_eq!(gen1.members[0].image_id, img0.id);
    assert_eq!(gen1.members[0].manifest_digest, m0);
    assert_eq!(
        gen1.members[0].required_host_tags,
        vec!["arch=arm64".to_string()]
    );
    assert_eq!(gen1.members[1].index, 1);
    assert_eq!(gen1.members[1].image_id, img1.id);
    assert_eq!(
        gen1.members[1].required_host_tags,
        vec!["arch=arm64".to_string(), "raspberrypi-4".to_string()]
    );

    // The group now reports its latest generation.
    let info: ImageGroupInfo = client
        .get(format!("{base}/image-groups/{}", group.id))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info.latest_generation, Some(1));

    // A second generation fully replaces the membership (just member 1 now) and
    // increments the generation number.
    let resp = client
        .post(format!("{base}/image-groups/{}/generations", group.id))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "members": [
            { "image_id": img1.id, "required_host_tags": [] },
        ] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let gen2: ImageGroupGenerationInfo = resp.json().await.unwrap();
    assert_eq!(gen2.generation, 2);
    assert_eq!(gen2.members.len(), 1);
    assert_eq!(gen2.members[0].image_id, img1.id);

    // Generation 1 is immutable and still inspectable after the replacement.
    let one: ImageGroupGenerationInfo = client
        .get(format!("{base}/image-groups/{}/generations/1", group.id))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(one.members.len(), 2);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn create_generation_rejects_an_unregistered_image(pool: PgPool) {
    let addr = spawn_with_registry(&pool, Arc::new(StubRegistry::default())).await;
    let client = http_client();
    let token = mock_login_token(&client, addr, "bob").await;
    let base = format!("http://{addr}/api/v1");

    let group: ImageGroupInfo = client
        .post(format!("{base}/image-groups"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "name": "empty" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // A member image id that was never registered is a 422.
    let resp = client
        .post(format!("{base}/image-groups/{}/generations", group.id))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "members": [
            { "image_id": Uuid::new_v4(), "required_host_tags": [] },
        ] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn image_group_permissions_are_enforced(pool: PgPool) {
    let addr = spawn_with_registry(&pool, Arc::new(StubRegistry::default())).await;
    let client = http_client();
    let bob_token = mock_login_token(&client, addr, "bob").await;
    let base = format!("http://{addr}/api/v1");

    // `bob` owns a group; `carol` initially has no access to it.
    let group: ImageGroupInfo = client
        .post(format!("{base}/image-groups"))
        .bearer_auth(&bob_token)
        .json(&serde_json::json!({ "name": "private" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let carol_token = mock_login_token(&client, addr, "carol").await;
    let carol = whoami(&client, addr, &carol_token).await;

    // Carol cannot even see the group (404, not 403 — existence is not leaked).
    let resp = client
        .get(format!("{base}/image-groups/{}", group.id))
        .bearer_auth(&carol_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);

    // Bob grants Carol `use`: she can now inspect it, but creating a generation
    // needs `manage` (403).
    let resp = client
        .post(format!("{base}/image-groups/{}/grants", group.id))
        .bearer_auth(&bob_token)
        .json(&serde_json::json!({ "subject_id": carol, "permission": "use" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);

    let resp = client
        .get(format!("{base}/image-groups/{}", group.id))
        .bearer_auth(&carol_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let resp = client
        .post(format!("{base}/image-groups/{}/generations", group.id))
        .bearer_auth(&carol_token)
        .json(&serde_json::json!({ "members": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);

    // Bob grants Carol `manage`: she may now append a generation (an empty
    // membership is a valid full replacement).
    let resp = client
        .post(format!("{base}/image-groups/{}/grants", group.id))
        .bearer_auth(&bob_token)
        .json(&serde_json::json!({ "subject_id": carol, "permission": "manage" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);

    let resp = client
        .post(format!("{base}/image-groups/{}/generations", group.id))
        .bearer_auth(&carol_token)
        .json(&serde_json::json!({ "members": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn enqueue_image_group_checks_use_and_freezes_generation(pool: PgPool) {
    let mut reg = StubRegistry::default();
    let m = reg.put(REGISTRY, REPO, image_manifest_bytes("member"));
    let addr = spawn_with_registry(&pool, Arc::new(reg)).await;
    let client = http_client();
    let bob_token = mock_login_token(&client, addr, "bob").await;
    let base = format!("http://{addr}/api/v1");

    let img = register_image(&client, &base, &bob_token, &m).await;
    let group = create_group(&client, &base, &bob_token, "ubuntu", false).await;

    // A group with no generation has nothing to freeze: enqueue is a 400.
    let resp = enqueue_group(&client, &base, &bob_token, group.id, None).await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

    add_generation(&client, &base, &bob_token, group.id, img.id).await;

    // `latest` is frozen onto the job (generation 1) at enqueue.
    let resp = enqueue_group(&client, &base, &bob_token, group.id, None).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let job_id = resp.json::<EnqueueJobResponse>().await.unwrap().job_id;
    let info: JobInfo = client
        .get(format!("{base}/jobs/{job_id}"))
        .bearer_auth(&bob_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(matches!(
        info.image,
        JobImageRef::ImageGroup { group_id, generation } if group_id == group.id && generation == 1
    ));

    // A non-existent explicit generation is a 400; the existing one is accepted.
    let resp = enqueue_group(&client, &base, &bob_token, group.id, Some(2)).await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let resp = enqueue_group(&client, &base, &bob_token, group.id, Some(1)).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    // `carol`, with no access to the private group, cannot enqueue against it
    // (403, existence not leaked).
    let carol_token = mock_login_token(&client, addr, "carol").await;
    let resp = enqueue_group(&client, &base, &carol_token, group.id, None).await;
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn public_grants_use_to_everyone(pool: PgPool) {
    let mut reg = StubRegistry::default();
    let m = reg.put(REGISTRY, REPO, image_manifest_bytes("member"));
    let addr = spawn_with_registry(&pool, Arc::new(reg)).await;
    let client = http_client();
    let bob_token = mock_login_token(&client, addr, "bob").await;
    let base = format!("http://{addr}/api/v1");

    let img = register_image(&client, &base, &bob_token, &m).await;
    // Created public at the start.
    let group = create_group(&client, &base, &bob_token, "public-ubuntu", true).await;
    assert!(group.public);
    add_generation(&client, &base, &bob_token, group.id, img.id).await;

    // `carol` holds no grant, yet a public group is visible and usable to her.
    let carol_token = mock_login_token(&client, addr, "carol").await;
    let resp = client
        .get(format!("{base}/image-groups/{}", group.id))
        .bearer_auth(&carol_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let resp = enqueue_group(&client, &base, &carol_token, group.id, None).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    // But `public` confers only `use`, never `manage`: carol cannot append a
    // generation, nor toggle the flag.
    let resp = client
        .post(format!("{base}/image-groups/{}/generations", group.id))
        .bearer_auth(&carol_token)
        .json(&serde_json::json!({ "members": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
    let resp = client
        .put(format!("{base}/image-groups/{}/public", group.id))
        .bearer_auth(&carol_token)
        .json(&serde_json::json!({ "public": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);

    // The owner toggles the group back to private; carol loses `use` again
    // (404 on inspect, 403 on enqueue).
    let resp = client
        .put(format!("{base}/image-groups/{}/public", group.id))
        .bearer_auth(&bob_token)
        .json(&serde_json::json!({ "public": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert!(!resp.json::<ImageGroupInfo>().await.unwrap().public);

    let resp = client
        .get(format!("{base}/image-groups/{}", group.id))
        .bearer_auth(&carol_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    let resp = enqueue_group(&client, &base, &carol_token, group.id, None).await;
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

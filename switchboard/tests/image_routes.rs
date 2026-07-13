//! End-to-end tests for the image-catalog REST API.
//!
//! Drive the real router over a loopback socket against ephemeral Postgres,
//! authenticating via the development mock-OAuth provider (no external service,
//! so multiple distinct users — `bob`, `carol` — are trivial to provision). The
//! registry the `/images` routes pull manifests from is a canned in-memory
//! [`StubRegistry`] (these tests have no Zot), so validation runs against real
//! OCI wire-format JSON without a registry daemon.
//!
//! Image *sets* are mutable, named switchboard entities: created empty, then
//! given membership by appending immutable, full-replacement generations whose
//! members are pre-registered images referenced by digest. No registry pull is
//! involved in set/generation creation.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::redirect::Policy;
use sha2::{Digest as _, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use treadmill_rs::api::switchboard::audit::AuditFeedResponse;
use treadmill_rs::api::switchboard::images::{
    ImageInfo, ImageSetGenerationInfo, ImageSetInfo, ImageSourceGrantInfo, ImageSourcePermission,
};
use treadmill_rs::api::switchboard::jobs::RestartPolicy;
use treadmill_rs::api::switchboard::jobs::{EnqueueJobResponse, JobImageRef, JobInfo};
use treadmill_rs::api::switchboard::{JobInitSpec, JobRequest, WhoAmIResponse};
use treadmill_rs::image::Digest;
use treadmill_switchboard::registry::{RegistryClient, RegistryError};
use treadmill_switchboard::serve::AppState;

mod common;
use common::{mock_login_token, spawn_server, test_config_mock};

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

/// Register a staged manifest by adding its first source via
/// `POST /images/{digest}/sources` and return its catalog view.
async fn register_image(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    digest: &Digest,
) -> ImageInfo {
    let resp = client
        .post(format!("{base}/images/{}/sources", digest.encoded()))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "registry": REGISTRY,
            "repository": REPO,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    resp.json().await.unwrap()
}

/// Create an empty image set via `POST /image-sets` and return its view.
async fn create_set(client: &reqwest::Client, base: &str, token: &str, name: &str) -> ImageSetInfo {
    let resp = client
        .post(format!("{base}/image-sets"))
        .bearer_auth(token)
        .json(&serde_json::json!({ "name": name }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    resp.json().await.unwrap()
}

/// The well-known `everyone` subject (see `SCHEMA.sql`): granting it `use` makes
/// a set public.
const EVERYONE_SUBJECT: Uuid = Uuid::from_u128(4);

/// Make a set public (or private again) by granting/revoking the `everyone`
/// subject `use` — the uniform replacement for a dedicated public flag. Returns
/// the response so the caller can assert on the authorization outcome.
async fn set_public(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    set: Uuid,
    public: bool,
) -> reqwest::Response {
    if public {
        client
            .post(format!("{base}/image-sets/{set}/grants"))
            .bearer_auth(token)
            .json(&serde_json::json!({ "subject_id": EVERYONE_SUBJECT, "permission": "use" }))
            .send()
            .await
            .unwrap()
    } else {
        client
            .delete(format!(
                "{base}/image-sets/{set}/grants/{EVERYONE_SUBJECT}/use"
            ))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
    }
}

/// Append a one-member generation referencing an image by digest.
async fn add_generation(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    set: Uuid,
    image: &Digest,
) {
    let resp = client
        .post(format!("{base}/image-sets/{set}/generations"))
        .bearer_auth(token)
        .json(&serde_json::json!({ "members": [
            { "manifest_digest": image.encoded(), "required_host_tags": [] },
        ] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
}

/// `POST /jobs` referencing an image set; returns the raw response so the
/// caller can assert on the status (the enqueue authorization is under test).
async fn enqueue_set(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    set: Uuid,
    generation: Option<u32>,
) -> reqwest::Response {
    let req = JobRequest {
        label: None,
        init_spec: JobInitSpec::ImageSet {
            set_id: set,
            generation,
        },
        owner: None,
        ssh_keys: vec![],
        restart_policy: RestartPolicy { max_restarts: 0 },
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

/// Grant `permission` on an image source to `subject` via
/// `POST /images/{digest}/sources/{source_id}/grants`. Returns the response so
/// the caller can assert on the authorization outcome. Granting the `everyone`
/// subject `use` makes the source public.
async fn grant_source(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    digest: &Digest,
    source_id: Uuid,
    subject: Uuid,
    permission: &str,
) -> reqwest::Response {
    client
        .post(format!(
            "{base}/images/{}/sources/{source_id}/grants",
            digest.encoded()
        ))
        .bearer_auth(token)
        .json(&serde_json::json!({ "subject_id": subject, "permission": permission }))
        .send()
        .await
        .unwrap()
}

/// `POST /jobs` referencing a concrete image; returns the raw response so the
/// caller can assert on the status (the source-availability gate is under test).
async fn enqueue_image_job(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    image: &Digest,
) -> reqwest::Response {
    let req = JobRequest {
        label: None,
        init_spec: JobInitSpec::Image {
            manifest_digest: *image,
        },
        owner: None,
        ssh_keys: vec![],
        restart_policy: RestartPolicy { max_restarts: 0 },
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
    let token = mock_login_token(&pool, &client, addr, "bob", true).await;
    let base = format!("http://{addr}/api/v1");

    // Adding the first source registers the image (201) and reports its single
    // external source; the title is cached off the manifest annotation.
    let resp = client
        .post(format!("{base}/images/{}/sources", digest.encoded()))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "registry": REGISTRY,
            "repository": REPO,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let info: ImageInfo = resp.json().await.unwrap();
    assert_eq!(info.manifest_digest, digest);
    assert_eq!(info.title.as_deref(), Some("Ubuntu test"));
    assert_eq!(info.sources.len(), 1);
    assert_eq!(info.sources[0].registry, REGISTRY);
    assert_eq!(info.sources[0].repository, REPO);
    assert_eq!(info.sources[0].status, "external");

    // Re-adding the same source to the known digest is idempotent (200, not a
    // duplicate).
    let resp = client
        .post(format!("{base}/images/{}/sources", digest.encoded()))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "registry": REGISTRY, "repository": REPO,
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
    assert_eq!(one.manifest_digest, info.manifest_digest);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn rejects_unknown_and_malformed_manifests(pool: PgPool) {
    // An empty registry: nothing staged, so the pull fails (502).
    let addr = spawn_with_registry(&pool, Arc::new(StubRegistry::default())).await;
    let client = http_client();
    let token = mock_login_token(&pool, &client, addr, "bob", true).await;
    let base = format!("http://{addr}/api/v1");

    let unknown = Digest::from_sha256([7u8; 32]);
    let resp = client
        .post(format!("{base}/images/{}/sources", unknown.encoded()))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "registry": REGISTRY, "repository": REPO,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_GATEWAY);

    // A staged document that is not a Treadmill artifact is rejected (422).
    let mut reg = StubRegistry::default();
    let bogus = reg.put(REGISTRY, REPO, br#"{"schemaVersion":2,"mediaType":"x","config":{"mediaType":"y","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","size":0},"layers":[]}"#.to_vec());
    let addr = spawn_with_registry(&pool, Arc::new(reg)).await;
    let token = mock_login_token(&pool, &client, addr, "bob", false).await;
    let base = format!("http://{addr}/api/v1");
    let resp = client
        .post(format!("{base}/images/{}/sources", bogus.encoded()))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "registry": REGISTRY, "repository": REPO,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
}

// -- image sets ---------------------------------------------------------------

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn create_group_append_generations_and_inspect(pool: PgPool) {
    // Two member images, registered as concrete images first.
    let mut reg = StubRegistry::default();
    let m0 = reg.put(REGISTRY, REPO, image_manifest_bytes("member 0"));
    let m1 = reg.put(REGISTRY, REPO, image_manifest_bytes("member 1"));
    let addr = spawn_with_registry(&pool, Arc::new(reg)).await;
    let client = http_client();
    let token = mock_login_token(&pool, &client, addr, "bob", true).await;
    let base = format!("http://{addr}/api/v1");

    register_image(&client, &base, &token, &m0).await;
    register_image(&client, &base, &token, &m1).await;

    // POST /image-sets creates an empty, named set with no generation yet.
    let resp = client
        .post(format!("{base}/image-sets"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "name": "ubuntu", "label": "Ubuntu set" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let set: ImageSetInfo = resp.json().await.unwrap();
    assert_eq!(set.name, "ubuntu");
    assert_eq!(set.label.as_deref(), Some("Ubuntu set"));
    assert_eq!(set.latest_generation, None);

    // POST a first generation: member 0 generic, member 1 more specific.
    let resp = client
        .post(format!("{base}/image-sets/{}/generations", set.id))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "members": [
            { "manifest_digest": m0.encoded(), "required_host_tags": ["arch=arm64"] },
            { "manifest_digest": m1.encoded(), "required_host_tags": ["arch=arm64", "raspberrypi-4"] },
        ] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let gen1: ImageSetGenerationInfo = resp.json().await.unwrap();
    assert_eq!(gen1.generation, 1);
    assert_eq!(gen1.members.len(), 2);
    // Members come back in array (index) order.
    assert_eq!(gen1.members[0].index, 0);
    assert_eq!(gen1.members[0].manifest_digest, m0);
    assert_eq!(
        gen1.members[0].required_host_tags,
        vec!["arch=arm64".to_string()]
    );
    assert_eq!(gen1.members[1].index, 1);
    assert_eq!(gen1.members[1].manifest_digest, m1);
    assert_eq!(
        gen1.members[1].required_host_tags,
        vec!["arch=arm64".to_string(), "raspberrypi-4".to_string()]
    );

    // The set now reports its latest generation.
    let info: ImageSetInfo = client
        .get(format!("{base}/image-sets/{}", set.id))
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
        .post(format!("{base}/image-sets/{}/generations", set.id))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "members": [
            { "manifest_digest": m1.encoded(), "required_host_tags": [] },
        ] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let gen2: ImageSetGenerationInfo = resp.json().await.unwrap();
    assert_eq!(gen2.generation, 2);
    assert_eq!(gen2.members.len(), 1);
    assert_eq!(gen2.members[0].manifest_digest, m1);

    // Generation 1 is immutable and still inspectable after the replacement.
    let one: ImageSetGenerationInfo = client
        .get(format!("{base}/image-sets/{}/generations/1", set.id))
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
    let token = mock_login_token(&pool, &client, addr, "bob", true).await;
    let base = format!("http://{addr}/api/v1");

    let set: ImageSetInfo = client
        .post(format!("{base}/image-sets"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "name": "empty" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // A member digest that was never registered is a 422.
    let unregistered = Digest::from_sha256([9u8; 32]);
    let resp = client
        .post(format!("{base}/image-sets/{}/generations", set.id))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "members": [
            { "manifest_digest": unregistered.encoded(), "required_host_tags": [] },
        ] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn image_set_permissions_are_enforced(pool: PgPool) {
    let addr = spawn_with_registry(&pool, Arc::new(StubRegistry::default())).await;
    let client = http_client();
    let bob_token = mock_login_token(&pool, &client, addr, "bob", true).await;
    let base = format!("http://{addr}/api/v1");

    // `bob` owns a set; `carol` initially has no access to it.
    let set: ImageSetInfo = client
        .post(format!("{base}/image-sets"))
        .bearer_auth(&bob_token)
        .json(&serde_json::json!({ "name": "private" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let carol_token = mock_login_token(&pool, &client, addr, "carol", true).await;
    let carol = whoami(&client, addr, &carol_token).await;

    // Carol cannot even see the set (404, not 403 — existence is not leaked).
    let resp = client
        .get(format!("{base}/image-sets/{}", set.id))
        .bearer_auth(&carol_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);

    // Bob grants Carol `use`: she can now inspect it, but creating a generation
    // needs `manage` (403).
    let resp = client
        .post(format!("{base}/image-sets/{}/grants", set.id))
        .bearer_auth(&bob_token)
        .json(&serde_json::json!({ "subject_id": carol, "permission": "use" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);

    let resp = client
        .get(format!("{base}/image-sets/{}", set.id))
        .bearer_auth(&carol_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let resp = client
        .post(format!("{base}/image-sets/{}/generations", set.id))
        .bearer_auth(&carol_token)
        .json(&serde_json::json!({ "members": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);

    // Bob grants Carol `manage`: she may now append a generation (an empty
    // membership is a valid full replacement).
    let resp = client
        .post(format!("{base}/image-sets/{}/grants", set.id))
        .bearer_auth(&bob_token)
        .json(&serde_json::json!({ "subject_id": carol, "permission": "manage" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);

    let resp = client
        .post(format!("{base}/image-sets/{}/generations", set.id))
        .bearer_auth(&carol_token)
        .json(&serde_json::json!({ "members": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn image_set_events_feed_records_acl_and_catalog_changes(pool: PgPool) {
    let mut reg = StubRegistry::default();
    let m = reg.put(REGISTRY, REPO, image_manifest_bytes("member"));
    let addr = spawn_with_registry(&pool, Arc::new(reg)).await;
    let client = http_client();
    let bob_token = mock_login_token(&pool, &client, addr, "bob", true).await;
    let base = format!("http://{addr}/api/v1");

    // Exercise the whole catalog/ACL surface on one set.
    register_image(&client, &base, &bob_token, &m).await;
    let set = create_set(&client, &base, &bob_token, "audited").await;
    add_generation(&client, &base, &bob_token, set.id, &m).await;

    let carol_token = mock_login_token(&pool, &client, addr, "carol", true).await;
    let carol = whoami(&client, addr, &carol_token).await;

    let grant = client
        .post(format!("{base}/image-sets/{}/grants", set.id))
        .bearer_auth(&bob_token)
        .json(&serde_json::json!({ "subject_id": carol, "permission": "use" }))
        .send()
        .await
        .unwrap();
    assert_eq!(grant.status(), reqwest::StatusCode::NO_CONTENT);

    // Making the set public is a grant to the `everyone` subject.
    let public = set_public(&client, &base, &bob_token, set.id, true).await;
    assert_eq!(public.status(), reqwest::StatusCode::NO_CONTENT);

    let revoke = client
        .delete(format!("{base}/image-sets/{}/grants/{carol}/use", set.id))
        .bearer_auth(&bob_token)
        .send()
        .await
        .unwrap();
    assert_eq!(revoke.status(), reqwest::StatusCode::NO_CONTENT);

    // The owner (a manager) sees the full set feed: one row per change above.
    // "Public" is an ordinary grant to the `everyone` subject, so it shows up as
    // a grant event, not a dedicated public-flag event.
    let feed: AuditFeedResponse = client
        .get(format!("{base}/image-sets/{}/events", set.id))
        .bearer_auth(&bob_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let types: Vec<&str> = feed.events.iter().map(|e| e.event_type.as_str()).collect();
    for expected in [
        "image_set_created.v1",
        "image_set_generation_created.v1",
        "image_set_grant_created.v1",
        "image_set_grant_revoked.v1",
    ] {
        assert!(
            types.contains(&expected),
            "missing {expected}: got {types:?}"
        );
    }

    // The feed is manage-gated: `carol` holds only `use` (via the now-public
    // set), so she can see the set but not its audit feed (403).
    let forbidden = client
        .get(format!("{base}/image-sets/{}/events", set.id))
        .bearer_auth(&carol_token)
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden.status(), reqwest::StatusCode::FORBIDDEN);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn enqueue_image_set_checks_use_and_freezes_generation(pool: PgPool) {
    let mut reg = StubRegistry::default();
    let m = reg.put(REGISTRY, REPO, image_manifest_bytes("member"));
    let addr = spawn_with_registry(&pool, Arc::new(reg)).await;
    let client = http_client();
    let bob_token = mock_login_token(&pool, &client, addr, "bob", true).await;
    let base = format!("http://{addr}/api/v1");

    register_image(&client, &base, &bob_token, &m).await;
    let set = create_set(&client, &base, &bob_token, "ubuntu").await;

    // A set with no generation has nothing to freeze: enqueue is a 400.
    let resp = enqueue_set(&client, &base, &bob_token, set.id, None).await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

    add_generation(&client, &base, &bob_token, set.id, &m).await;

    // `latest` is frozen onto the job (generation 1) at enqueue.
    let resp = enqueue_set(&client, &base, &bob_token, set.id, None).await;
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
        JobImageRef::ImageSet { set_id, generation } if set_id == set.id && generation == 1
    ));

    // A non-existent explicit generation is a 400; the existing one is accepted.
    let resp = enqueue_set(&client, &base, &bob_token, set.id, Some(2)).await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let resp = enqueue_set(&client, &base, &bob_token, set.id, Some(1)).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    // `carol`, with no access to the private set, cannot enqueue against it
    // (403, existence not leaked).
    let carol_token = mock_login_token(&pool, &client, addr, "carol", true).await;
    let resp = enqueue_set(&client, &base, &carol_token, set.id, None).await;
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn public_grants_use_to_everyone(pool: PgPool) {
    let mut reg = StubRegistry::default();
    let m = reg.put(REGISTRY, REPO, image_manifest_bytes("member"));
    let addr = spawn_with_registry(&pool, Arc::new(reg)).await;
    let client = http_client();
    let bob_token = mock_login_token(&pool, &client, addr, "bob", true).await;
    let base = format!("http://{addr}/api/v1");

    let img = register_image(&client, &base, &bob_token, &m).await;
    // Made public by granting the `everyone` subject `use`.
    let set = create_set(&client, &base, &bob_token, "public-ubuntu").await;
    let resp = set_public(&client, &base, &bob_token, set.id, true).await;
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    add_generation(&client, &base, &bob_token, set.id, &m).await;

    // `carol` holds no grant, yet a public set is visible to her.
    let carol_token = mock_login_token(&pool, &client, addr, "carol", true).await;
    let resp = client
        .get(format!("{base}/image-sets/{}", set.id))
        .bearer_auth(&carol_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    // The set grant is necessary but not sufficient to enqueue: the member's
    // only source is bob's private one, so carol cannot source the image (422).
    let resp = enqueue_set(&client, &base, &carol_token, set.id, None).await;
    assert_eq!(resp.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);

    // Bob makes the member's source public too; now carol can enqueue.
    let resp = grant_source(
        &client,
        &base,
        &bob_token,
        &m,
        img.sources[0].id,
        EVERYONE_SUBJECT,
        "use",
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    let resp = enqueue_set(&client, &base, &carol_token, set.id, None).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    // But the `everyone` grant confers only `use`, never `manage`: carol cannot
    // append a generation, nor change the set's grants (make it private).
    let resp = client
        .post(format!("{base}/image-sets/{}/generations", set.id))
        .bearer_auth(&carol_token)
        .json(&serde_json::json!({ "members": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
    let resp = set_public(&client, &base, &carol_token, set.id, false).await;
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);

    // The owner revokes the `everyone` grant; carol loses `use` again (404 on
    // inspect, 403 on enqueue).
    let resp = set_public(&client, &base, &bob_token, set.id, false).await;
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);

    let resp = client
        .get(format!("{base}/image-sets/{}", set.id))
        .bearer_auth(&carol_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    let resp = enqueue_set(&client, &base, &carol_token, set.id, None).await;
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

// -- image sources ----------------------------------------------------------------

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn image_source_grants_gate_visibility_and_use(pool: PgPool) {
    let mut reg = StubRegistry::default();
    let m = reg.put(REGISTRY, REPO, image_manifest_bytes("source acl"));
    let addr = spawn_with_registry(&pool, Arc::new(reg)).await;
    let client = http_client();
    let bob_token = mock_login_token(&pool, &client, addr, "bob", true).await;
    let bob = whoami(&client, addr, &bob_token).await;
    let base = format!("http://{addr}/api/v1");

    // Bob registers the image: he owns its only source, holding both permissions
    // on it implicitly.
    let info = register_image(&client, &base, &bob_token, &m).await;
    assert_eq!(info.sources.len(), 1);
    let source_id = info.sources[0].id;
    assert_eq!(info.sources[0].owner_id, Some(bob));
    for p in [ImageSourcePermission::Use, ImageSourcePermission::Manage] {
        assert!(
            info.sources[0].permissions.contains(&p),
            "owner must hold {p:?} implicitly"
        );
    }

    // Carol has no usable source, so the image does not exist for her: 404 on
    // inspect (existence not leaked) and absent from her list.
    let carol_token = mock_login_token(&pool, &client, addr, "carol", true).await;
    let carol = whoami(&client, addr, &carol_token).await;
    let resp = client
        .get(format!("{base}/images/{}", m.encoded()))
        .bearer_auth(&carol_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    let list: Vec<ImageInfo> = client
        .get(format!("{base}/images"))
        .bearer_auth(&carol_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(list.is_empty(), "no usable source: nothing to list");

    // Bob grants carol `use`: the image becomes visible to her, and her surfaced
    // permissions on the source are exactly `use`.
    let resp = grant_source(&client, &base, &bob_token, &m, source_id, carol, "use").await;
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    let one: ImageInfo = client
        .get(format!("{base}/images/{}", m.encoded()))
        .bearer_auth(&carol_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(one.sources[0].permissions, vec![ImageSourcePermission::Use]);

    // `use` does not confer management: granting, listing grants, and deleting
    // the source are all 403 for carol.
    let resp = grant_source(&client, &base, &carol_token, &m, source_id, carol, "manage").await;
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
    let resp = client
        .get(format!(
            "{base}/images/{}/sources/{source_id}/grants",
            m.encoded()
        ))
        .bearer_auth(&carol_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
    let resp = client
        .delete(format!("{base}/images/{}/sources/{source_id}", m.encoded()))
        .bearer_auth(&carol_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);

    // Bob (manage) sees the grant listed; revoking it hides the image from carol
    // again.
    let grants: Vec<ImageSourceGrantInfo> = client
        .get(format!(
            "{base}/images/{}/sources/{source_id}/grants",
            m.encoded()
        ))
        .bearer_auth(&bob_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        grants
            .iter()
            .any(|g| g.subject_id == carol && g.permission == ImageSourcePermission::Use),
        "the grant to carol must be listed, got {grants:?}"
    );
    let resp = client
        .delete(format!(
            "{base}/images/{}/sources/{source_id}/grants/{carol}/use",
            m.encoded()
        ))
        .bearer_auth(&bob_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    let resp = client
        .get(format!("{base}/images/{}", m.encoded()))
        .bearer_auth(&carol_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);

    // The owner deletes his source: the image keeps existing as a manifest
    // identity, but with no sources left nobody — bob included — has a usable
    // source, so even his inspect is now a 404.
    let resp = client
        .delete(format!("{base}/images/{}/sources/{source_id}", m.encoded()))
        .bearer_auth(&bob_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    let resp = client
        .get(format!("{base}/images/{}", m.encoded()))
        .bearer_auth(&bob_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn enqueue_concrete_image_requires_usable_source(pool: PgPool) {
    let mut reg = StubRegistry::default();
    let m = reg.put(REGISTRY, REPO, image_manifest_bytes("concrete gate"));
    let addr = spawn_with_registry(&pool, Arc::new(reg)).await;
    let client = http_client();
    let bob_token = mock_login_token(&pool, &client, addr, "bob", true).await;
    let base = format!("http://{addr}/api/v1");
    let img = register_image(&client, &base, &bob_token, &m).await;

    // The source's owner can enqueue against the image.
    let resp = enqueue_image_job(&client, &base, &bob_token, &m).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    // Carol cannot source it: 422 (the concrete-image path has no set-style
    // pre-check, so this gate is the only thing standing between her and a job
    // that would only image_error at dispatch).
    let carol_token = mock_login_token(&pool, &client, addr, "carol", true).await;
    let resp = enqueue_image_job(&client, &base, &carol_token, &m).await;
    assert_eq!(resp.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);

    // A public source (`everyone` granted `use`) unblocks her.
    let resp = grant_source(
        &client,
        &base,
        &bob_token,
        &m,
        img.sources[0].id,
        EVERYONE_SUBJECT,
        "use",
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    let resp = enqueue_image_job(&client, &base, &carol_token, &m).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
}

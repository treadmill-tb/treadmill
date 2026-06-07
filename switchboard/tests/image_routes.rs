//! End-to-end tests for the image-catalog REST API.
//!
//! These mirror `user_routes.rs`: provision a user via the GitHub OAuth callback
//! against a `wiremock` stand-in, then drive the `/images` and `/image-groups`
//! routes over HTTP against the real router and an ephemeral Postgres. The
//! registry the routes pull manifests from is a canned in-memory
//! [`StubRegistry`] (these tests have no Zot), so validation runs against real
//! OCI wire-format JSON without a registry daemon.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use sha2::{Digest as _, Sha256};
use sqlx::PgPool;
use tokio::net::TcpListener;
use treadmill_rs::api::switchboard::LoginResponse;
use treadmill_rs::api::switchboard::images::{ImageGroupInfo, ImageInfo};
use treadmill_rs::image::Digest;
use treadmill_switchboard::config::{
    DatabaseConfig, GitHubOAuthConfig, LogConfig, OAuthConfig, ServerConfig, ServiceConfig,
    SwitchboardConfig,
};
use treadmill_switchboard::registry::{RegistryClient, RegistryError};
use treadmill_switchboard::routes::build_router;
use treadmill_switchboard::serve::AppState;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// -- canned registry ------------------------------------------------------------

/// An in-memory registry mapping `(registry, repository, digest)` to the exact
/// manifest/index bytes the test staged.
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

/// An image-group index with one member at `member_digest`, eligible on hosts
/// carrying all of `required_host_tags` (a comma-separated list).
fn image_index_bytes(member_digest: &Digest, required_host_tags: &str) -> Vec<u8> {
    format!(
        r#"{{
          "schemaVersion": 2,
          "mediaType": "application/vnd.oci.image.index.v1+json",
          "artifactType": "application/vnd.treadmill.image-group.v1+json",
          "manifests": [
            {{ "mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": "{member}", "size": 111,
               "annotations": {{ "ci.treadmill.required-host-tags": "{required_host_tags}" }} }}
          ]
        }}"#,
        member = member_digest.encoded(),
    )
    .into_bytes()
}

// -- login harness (mirrors user_routes.rs) -------------------------------------

async fn mount_github(server: &MockServer) {
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
    Mock::given(method("GET"))
        .and(path("/user/memberships/orgs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(server)
        .await;
}

fn test_config(gh_uri: &str) -> SwitchboardConfig {
    SwitchboardConfig {
        database: DatabaseConfig {
            host: "unused".to_string(),
            port: None,
            database: "unused".to_string(),
            user: "unused".to_string(),
            auth: None,
        },
        server: ServerConfig {
            bind_address: "127.0.0.1:0".parse().unwrap(),
            testing_only_tls_config: None,
            trusted_proxy_headers: Vec::new(),
        },
        service: ServiceConfig {
            default_token_timeout: chrono::Duration::hours(1),
            default_job_timeout: chrono::Duration::hours(1),
            default_queue_timeout: chrono::Duration::hours(1),
            match_interval: chrono::Duration::seconds(1),
            host_liveness_timeout: chrono::Duration::seconds(30),
            supervisor_ping_interval: std::time::Duration::from_secs(30),
            supervisor_pong_dead: std::time::Duration::from_secs(60),
        },
        log: LogConfig {
            use_tokio_console_subscriber: false,
        },
        oauth: OAuthConfig {
            github: Some(GitHubOAuthConfig {
                client_id: "test-client".to_string(),
                client_secret: "test-secret".to_string(),
                redirect_url: "http://localhost/api/v1/auth/github/callback".to_string(),
                auth_url: "http://localhost/login/oauth/authorize".to_string(),
                token_url: format!("{gh_uri}/login/oauth/access_token"),
                api_base_url: gh_uri.to_string(),
                scopes: vec![
                    "read:user".to_string(),
                    "user:email".to_string(),
                    "read:org".to_string(),
                ],
            }),
        },
    }
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

async fn run_login(client: &reqwest::Client, addr: SocketAddr, pool: &PgPool) -> LoginResponse {
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
    let cb_resp = client
        .get(format!(
            "http://{addr}/api/v1/auth/github/callback?code=CANNED_CODE&state={state}"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(cb_resp.status(), reqwest::StatusCode::OK);
    cb_resp.json().await.unwrap()
}

/// Provision a user via login and spawn a server backed by `registry`.
async fn login_with_registry(
    pool: &PgPool,
    registry: Arc<dyn RegistryClient>,
) -> (SocketAddr, String, MockServer) {
    let gh = MockServer::start().await;
    mount_github(&gh).await;
    let state = AppState::with_registry(pool.clone(), test_config(&gh.uri()), registry);
    let addr = spawn_server(state).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let session = run_login(&client, addr, pool).await;
    (addr, session.token.encode_for_http(), gh)
}

const REGISTRY: &str = "registry.example.com:5000";
const REPO: &str = "u/octocat/ubuntu";

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn register_list_and_inspect_image(pool: PgPool) {
    let mut reg = StubRegistry::default();
    let manifest = image_manifest_bytes("Ubuntu test");
    let digest = reg.put(REGISTRY, REPO, manifest);
    let (addr, token, _gh) = login_with_registry(&pool, Arc::new(reg)).await;
    let client = reqwest::Client::new();
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
    let (addr, token, _gh) = login_with_registry(&pool, Arc::new(StubRegistry::default())).await;
    let client = reqwest::Client::new();
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
    let (addr, token, _gh) = login_with_registry(&pool, Arc::new(reg)).await;
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

#[sqlx::test]
#[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
async fn register_and_inspect_image_group(pool: PgPool) {
    let mut reg = StubRegistry::default();
    let member = image_manifest_bytes("group member");
    let member_digest = reg.put(REGISTRY, REPO, member);
    let index = image_index_bytes(&member_digest, "arch=arm64, raspberrypi-4");
    let index_digest = reg.put(REGISTRY, REPO, index);
    let (addr, token, _gh) = login_with_registry(&pool, Arc::new(reg)).await;
    let client = reqwest::Client::new();
    let base = format!("http://{addr}/api/v1");

    let resp = client
        .post(format!("{base}/image-groups"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "registry": REGISTRY, "repository": REPO, "index_digest": index_digest.encoded(),
            "label": "ubuntu group",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let info: ImageGroupInfo = resp.json().await.unwrap();
    assert_eq!(info.index_digest, index_digest);
    assert_eq!(info.members.len(), 1);
    let m = &info.members[0];
    assert_eq!(m.manifest_digest, member_digest);
    assert_eq!(
        m.required_host_tags,
        vec!["arch=arm64".to_string(), "raspberrypi-4".to_string()]
    );

    // The member was also registered as a standalone image (same repo).
    let member_img: ImageInfo = client
        .get(format!("{base}/images/{}", member_digest.encoded()))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(member_img.manifest_digest, member_digest);

    // GET /image-groups/{digest} inspects the group.
    let one: ImageGroupInfo = client
        .get(format!("{base}/image-groups/{}", index_digest.encoded()))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(one.id, info.id);
}

//! Read-only client of the per-server Zot store daemon.
//!
//! Under the OCI image migration (`doc/oci-image-migration-plan.md` §6/§7) a
//! single per-server **Zot daemon owns the local store** — it is the only
//! writer. Supervisors never write it: they ask the daemon to make a digest
//! present (a pull-through from a registry location, or a cache hit) and then
//! open the daemon's on-disk blob files **read-only**, pointing `qemu` /
//! `qemu-nbd` straight at them. One on-disk copy, shared page cache (D7/D8).
//!
//! Zot lays its storage out as one OCI image layout per repository:
//!
//! ```text
//! <store_root>/<repository>/blobs/sha256/<hex>
//! <store_root>/<repository>/{index.json, oci-layout}
//! ```
//!
//! so [`OciStore::blob_path`] resolves a blob within the repository it was
//! pulled under. Ingest, dedup, and GC are the daemon's serialized concern; this
//! type only drives a pull and then reads files.
//!
//! "Making a digest present" is done over the OCI Distribution protocol against
//! the **local** daemon: pulling the manifest triggers the daemon's pull-through
//! sync (when it is configured as a cache), and any blob still absent afterwards
//! is fetched with a blob pull that streams to a sink — the bytes land in the
//! daemon's store, not a second copy here. Every candidate location is tried in
//! order so a supervisor fails over when one is unavailable (D16).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use oci_client::{
    Reference,
    client::{Client, ClientConfig, ClientProtocol},
    secrets::RegistryAuth,
};
use oci_spec::image::{ImageManifest, MediaType};
use serde::Deserialize;
use tracing::{Level, event, instrument};

use treadmill_rs::image::Digest;

/// Configuration for the supervisor's [`OciStore`] client: the local Zot
/// daemon's authority and the on-disk root of its storage (§6/§7).
#[derive(Clone, Debug, Deserialize)]
pub struct OciStoreConfig {
    /// Authority (`host:port`, no scheme) of the local Zot daemon.
    pub registry: String,
    /// Filesystem root of the daemon's storage (read-only from the supervisor).
    pub store_root: PathBuf,
}

/// The image-store operations a supervisor's job state machine depends on.
///
/// Injectable (the supervisors hold `Arc<dyn ImageStore>`) so the state machine
/// can be driven by tests with a stub store, and so the production path uses the
/// OCI-backed [`OciStore`]. The methods mirror [`OciStore`]'s inherent API; see
/// `doc/oci-image-migration-plan.md` §6.
#[async_trait]
pub trait ImageStore: std::fmt::Debug + Send + Sync {
    /// Ensure the image manifest `digest` and its blob closure are present in
    /// the local daemon's store, trying each location in order. Returns the
    /// repository the image is present under.
    async fn ensure_present(&self, digest: &Digest, locations: &[Location]) -> Result<String>;

    /// Read and parse the (already-present) OCI image manifest `digest` from the
    /// daemon's store under `repository`.
    async fn manifest(&self, repository: &str, digest: &Digest) -> Result<ImageManifest>;

    /// Filesystem path at which `digest`'s bytes can be opened read-only within
    /// `repository`.
    fn blob_path(&self, repository: &str, digest: &Digest) -> PathBuf;

    /// Pin the (already-present) manifest `digest` against the daemon's garbage
    /// collector for the duration of job `job_id` (an in-use *lease*, plan
    /// §7.3). Pinning the manifest transitively protects its config and every
    /// layer blob via GC reachability. Idempotent: pinning twice is harmless.
    ///
    /// The default is a no-op, for stores with no garbage collector (e.g. test
    /// stubs); [`OciStore`] overrides it with a real Zot reference.
    async fn pin(&self, _repository: &str, _digest: &Digest, _job_id: &str) -> Result<()> {
        Ok(())
    }

    /// Release the in-use lease pinned by [`ImageStore::pin`] for `job_id`
    /// (plan §7.3). After this the manifest is GC-eligible iff no other lease or
    /// tag references it. Idempotent: releasing an absent lease succeeds.
    ///
    /// The default is a no-op (see [`ImageStore::pin`]).
    async fn unpin(&self, _repository: &str, _job_id: &str) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl ImageStore for OciStore {
    async fn ensure_present(&self, digest: &Digest, locations: &[Location]) -> Result<String> {
        OciStore::ensure_present(self, digest, locations).await
    }

    async fn manifest(&self, repository: &str, digest: &Digest) -> Result<ImageManifest> {
        OciStore::manifest(self, repository, digest).await
    }

    fn blob_path(&self, repository: &str, digest: &Digest) -> PathBuf {
        OciStore::blob_path(self, repository, digest)
    }

    async fn pin(&self, repository: &str, digest: &Digest, job_id: &str) -> Result<()> {
        OciStore::pin(self, repository, digest, job_id).await
    }

    async fn unpin(&self, repository: &str, job_id: &str) -> Result<()> {
        OciStore::unpin(self, repository, job_id).await
    }
}

/// One place the local daemon can be asked to source an image from.
///
/// In the direct-read model the supervisor always talks to its *local* Zot; the
/// `repository` is the path it pulls under (which the daemon's sync config maps
/// to the appropriate upstream). Multiple locations let a supervisor fail over
/// when one is unavailable — every location serves the same digest, so any that
/// succeeds is interchangeable (D12/D16).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Location {
    /// Repository path on the local daemon, e.g. `treadmill/ubuntu-22.04`.
    pub repository: String,
    // TODO(Phase 5, D11): a per-repository bearer pull token. Anonymous for now.
}

impl Location {
    pub fn new(repository: impl Into<String>) -> Self {
        Location {
            repository: repository.into(),
        }
    }
}

/// A read-only client of the per-server Zot daemon.
pub struct OciStore {
    /// Registry authority of the local daemon (host:port, no scheme), as the
    /// OCI [`Reference`] type expects it.
    registry: String,
    /// Filesystem root of the daemon's storage (ro from the supervisor's view).
    store_root: PathBuf,
    client: Client,
    /// Plain HTTP client for the lease operations (manifest PUT/DELETE), which
    /// `oci-client` does not expose. Talks the OCI Distribution protocol to the
    /// same local daemon over loopback.
    http: reqwest::Client,
}

// `oci_client::Client` is not `Debug`; project only the fields that identify the
// store (the supervisors require `ImageStore: Debug`).
impl std::fmt::Debug for OciStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OciStore")
            .field("registry", &self.registry)
            .field("store_root", &self.store_root)
            .finish_non_exhaustive()
    }
}

impl OciStore {
    /// Create a store client for the local daemon at `registry` (a `host:port`
    /// authority served over plain HTTP) whose storage lives at `store_root`.
    pub fn new(registry: impl Into<String>, store_root: impl Into<PathBuf>) -> Self {
        // The local daemon is reached over loopback HTTP; there is no TLS to
        // verify and no standing credentials (token-gating is D11/Phase 5).
        let client = Client::new(ClientConfig {
            protocol: ClientProtocol::Http,
            ..Default::default()
        });
        OciStore {
            registry: registry.into(),
            store_root: store_root.into(),
            client,
            http: reqwest::Client::new(),
        }
    }

    /// Path at which `digest`'s bytes can be opened (read-only) within
    /// `repository`. The blob is only guaranteed to exist after a successful
    /// [`OciStore::ensure_present`] naming the same repository.
    pub fn blob_path(&self, repository: &str, digest: &Digest) -> PathBuf {
        self.store_root
            .join(repository)
            .join("blobs")
            .join("sha256")
            .join(digest.hex())
    }

    /// Whether `digest` is already a file in the daemon's store under `repository`.
    async fn blob_on_disk(&self, repository: &str, digest: &Digest) -> bool {
        tokio::fs::metadata(self.blob_path(repository, digest))
            .await
            .map(|m| m.is_file())
            .unwrap_or(false)
    }

    /// Ensure the image manifest `digest` and its whole blob closure are present
    /// in the local daemon's store, trying each location in order and failing
    /// over on error (D16). Returns the repository the image is now present
    /// under, for use with [`OciStore::blob_path`] / [`OciStore::manifest`].
    ///
    /// Concurrent calls for the same digest are safe: ingest is serialized inside
    /// the daemon and is idempotent (content-addressed).
    #[instrument(skip(self, locations), fields(%digest))]
    pub async fn ensure_present(&self, digest: &Digest, locations: &[Location]) -> Result<String> {
        if locations.is_empty() {
            bail!("no locations supplied to ensure_present for {digest}");
        }

        let mut failures = Vec::new();
        for location in locations {
            match self.ensure_from(&location.repository, digest).await {
                Ok(()) => {
                    event!(
                        Level::DEBUG,
                        repository = %location.repository,
                        "image present in local store",
                    );
                    return Ok(location.repository.clone());
                }
                Err(e) => {
                    event!(
                        Level::WARN,
                        repository = %location.repository,
                        error = ?e,
                        "location failed; trying next",
                    );
                    failures.push(format!("{}: {e:#}", location.repository));
                }
            }
        }

        bail!(
            "could not make {digest} present from any of {} location(s): {}",
            locations.len(),
            failures.join("; "),
        )
    }

    /// Drive the local daemon to make `digest`'s closure present under a single
    /// repository.
    async fn ensure_from(&self, repository: &str, digest: &Digest) -> Result<()> {
        let reference = Reference::with_digest(
            self.registry.clone(),
            repository.to_string(),
            digest.encoded(),
        );

        // Pulling the manifest triggers the daemon's pull-through sync (when it
        // is a cache) and tells us the blob closure to ensure. The daemon
        // verifies and stores it; we keep only the descriptor list.
        let (manifest, _resolved) = self
            .client
            .pull_image_manifest(&reference, &RegistryAuth::Anonymous)
            .await
            .with_context(|| format!("pulling manifest {digest} from {repository}"))?;

        // The config blob plus every layer must be on disk for the runtime to
        // open them. A blob already cached is left untouched (no re-download);
        // an absent one is fetched with a streaming pull whose bytes go to the
        // daemon's store, not a second copy here. `pull_blob` verifies the
        // streamed content against the descriptor digest.
        let blob_digests = std::iter::once(&manifest.config)
            .chain(manifest.layers.iter())
            .map(|desc| {
                desc.digest
                    .parse::<Digest>()
                    .with_context(|| format!("descriptor digest {:?}", desc.digest))
            })
            .collect::<Result<Vec<_>>>()?;

        for blob in &blob_digests {
            if self.blob_on_disk(repository, blob).await {
                continue;
            }
            self.client
                .pull_blob(&reference, blob.encoded().as_str(), tokio::io::sink())
                .await
                .with_context(|| format!("pulling blob {blob} from {repository}"))?;

            if !self.blob_on_disk(repository, blob).await {
                bail!("daemon did not persist blob {blob} for {repository}");
            }
        }

        // The manifest itself is stored as a blob; confirm the daemon kept it so
        // `manifest()` and the digest pin (Phase 3) can rely on it.
        if !self.blob_on_disk(repository, digest).await {
            bail!("daemon did not persist manifest {digest} for {repository}");
        }

        Ok(())
    }

    /// Read and parse the (already-present) image manifest `digest` from the
    /// daemon's store under `repository`.
    #[instrument(skip(self), fields(%digest))]
    pub async fn manifest(&self, repository: &str, digest: &Digest) -> Result<ImageManifest> {
        let path = self.blob_path(repository, digest);
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("reading manifest {digest} at {}", path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing manifest {digest} as an OCI image manifest"))
    }

    /// Read a blob's bytes directly from the daemon's store. Intended for small
    /// blobs (manifests, configs) and tests; large disk blobs are opened by
    /// `qemu`/`qemu-nbd` via [`OciStore::blob_path`], not read into memory here.
    pub async fn read_blob(&self, repository: &str, digest: &Digest) -> Result<Vec<u8>> {
        let path = self.blob_path(repository, digest);
        tokio::fs::read(&path)
            .await
            .with_context(|| format!("reading blob {digest} at {}", path.display()))
    }

    /// The local daemon's manifest URL for `reference` (a tag or digest) under
    /// `repository`. The daemon is reached over loopback HTTP (`new` fixes the
    /// protocol to HTTP); token-gating is D11/Phase 5.
    fn manifest_url(&self, repository: &str, reference: &str) -> String {
        format!(
            "http://{}/v2/{}/manifests/{}",
            self.registry, repository, reference
        )
    }

    /// Pin the (already-present) manifest `digest` against the daemon's GC for
    /// the duration of job `job_id` — the in-use *lease* of plan §7.3.
    ///
    /// Implemented as a Zot **reference**: we push the manifest's own bytes back
    /// under a per-job [`inuse_tag`] in the same `repository`. The bytes are
    /// content-addressed, so the daemon recognises the identical digest and only
    /// records a new tag pointing at the existing manifest — **no blob is
    /// copied**. Because the manifest is now tagged, the daemon's GC keeps it in
    /// `index.json`, which transitively retains its config and every layer blob
    /// (GC reachability). Idempotent: re-pinning re-PUTs the same tag.
    #[instrument(skip(self), fields(%digest))]
    pub async fn pin(&self, repository: &str, digest: &Digest, job_id: &str) -> Result<()> {
        // The on-disk manifest blob *is* the canonical bytes whose sha256 is
        // `digest`; re-pushing them keeps the daemon's digest identical, so the
        // tag attaches to the existing manifest rather than ingesting a copy.
        let bytes = self
            .read_blob(repository, digest)
            .await
            .with_context(|| format!("reading manifest {digest} to pin it"))?;

        // PUT must echo the manifest's own media type; fall back to the OCI
        // image-manifest type if the blob somehow lacks the field.
        let media_type = serde_json::from_slice::<MediaTypeProbe>(&bytes)
            .ok()
            .and_then(|p| p.media_type)
            .unwrap_or_else(|| MediaType::ImageManifest.to_string());

        let tag = inuse_tag(job_id);
        let url = self.manifest_url(repository, &tag);
        let resp = self
            .http
            .put(&url)
            .header(reqwest::header::CONTENT_TYPE, media_type)
            .body(bytes)
            .send()
            .await
            .with_context(|| format!("pinning {digest} as {tag} in {repository}"))?;
        if !resp.status().is_success() {
            bail!(
                "pinning {digest} as {tag} in {repository} failed: HTTP {}",
                resp.status(),
            );
        }
        event!(Level::DEBUG, %tag, repository, "pinned in-use lease");
        Ok(())
    }

    /// Release the in-use lease pinned by [`OciStore::pin`] for `job_id` (plan
    /// §7.3): delete the per-job [`inuse_tag`] from `repository`.
    ///
    /// Deleting the tag leaves the manifest GC-eligible only when no other
    /// reference names it (another job's lease, or a catalog tag); the daemon's
    /// GC then reclaims the now-unreferenced closure. Idempotent: a missing tag
    /// (`404`) is treated as already released.
    #[instrument(skip(self))]
    pub async fn unpin(&self, repository: &str, job_id: &str) -> Result<()> {
        let tag = inuse_tag(job_id);
        let url = self.manifest_url(repository, &tag);
        let resp = self
            .http
            .delete(&url)
            .send()
            .await
            .with_context(|| format!("releasing lease {tag} in {repository}"))?;
        let status = resp.status();
        if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
            event!(Level::DEBUG, %tag, repository, "released in-use lease");
            Ok(())
        } else {
            bail!("releasing lease {tag} in {repository} failed: HTTP {status}");
        }
    }
}

/// The per-job in-use tag pinning a manifest against the daemon's GC (plan
/// §7.3). Job ids are UUIDs (`hex` + `-`), which are valid OCI tag components,
/// so they embed verbatim under an `inuse-` prefix.
fn inuse_tag(job_id: &str) -> String {
    format!("inuse-{job_id}")
}

/// Minimal probe for a manifest blob's `mediaType`, to set the `Content-Type`
/// when re-pushing it as a lease tag (see [`OciStore::pin`]).
#[derive(Deserialize)]
struct MediaTypeProbe {
    #[serde(rename = "mediaType")]
    media_type: Option<String>,
}

/// Resolve a `host:port` authority from a base URL or bare authority, for
/// building [`OciStore::new`] from configuration that may include a scheme.
pub fn registry_authority(endpoint: &str) -> Result<String> {
    let authority = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .unwrap_or(endpoint)
        .trim_end_matches('/');
    if authority.is_empty() {
        return Err(anyhow!("empty registry endpoint"));
    }
    Ok(authority.to_string())
}

/// Convenience: the blob path for a digest under a repository in a store rooted
/// at `store_root`, without constructing an [`OciStore`] (used by tooling/tests).
pub fn blob_path_in(store_root: &Path, repository: &str, digest: &Digest) -> PathBuf {
    store_root
        .join(repository)
        .join("blobs")
        .join("sha256")
        .join(digest.hex())
}

// Integration tests that drive a real child Zot daemon (plan §12.3). They need
// `zot` and `skopeo` on `PATH` and the `tiny-efi` fixture layout in
// `TINY_EFI_IMAGE`; with any of those absent they skip, so the workspace
// `nextest` check passes them over and the dedicated `oci-store` Nix check
// (which supplies all three) runs them for real.
#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Read;
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use sha2::{Digest as _, Sha256};
    use tempfile::TempDir;

    const REPO: &str = "treadmill/tiny-efi";

    /// External tools + fixture the tests need; `None` means skip.
    struct Reqs {
        zot: PathBuf,
        skopeo: PathBuf,
        fixture: PathBuf,
    }

    fn which(bin: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(bin))
            .find(|candidate| candidate.is_file())
    }

    fn reqs() -> Option<Reqs> {
        let fixture = std::env::var_os("TINY_EFI_IMAGE").map(PathBuf::from)?;
        Some(Reqs {
            zot: which("zot")?,
            skopeo: which("skopeo")?,
            fixture,
        })
    }

    macro_rules! skip_unless_available {
        () => {
            match reqs() {
                Some(r) => r,
                None => {
                    eprintln!("zot/skopeo/TINY_EFI_IMAGE unavailable; skipping oci_store test");
                    return;
                }
            }
        };
    }

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// A running child Zot; killed on drop.
    struct Zot {
        child: Child,
        port: u16,
        store: PathBuf,
        _dir: TempDir,
    }

    impl Drop for Zot {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    impl Zot {
        /// Authority (`host:port`) for `OciStore::new` / `Reference`.
        fn authority(&self) -> String {
            format!("127.0.0.1:{}", self.port)
        }

        /// Start Zot with a temp store. When `sync_from` is set, configure it as
        /// an on-demand pull-through cache of that upstream port.
        fn start(zot_bin: &Path, sync_from: Option<u16>) -> Zot {
            Self::spawn(zot_bin, |store, port| {
                let sync = match sync_from {
                    Some(up) => format!(
                        r#","extensions":{{"sync":{{"registries":[
                            {{"urls":["http://127.0.0.1:{up}"],"onDemand":true,
                              "tlsVerify":false,"content":[{{"prefix":"**"}}]}}]}}}}"#
                    ),
                    None => String::new(),
                };
                format!(
                    r#"{{"storage":{{"rootDirectory":"{store}","dedupe":true}},
                        "http":{{"address":"127.0.0.1","port":"{port}"}},
                        "log":{{"level":"error"}}{sync}}}"#,
                    store = store.display(),
                )
            })
        }

        /// Start Zot with garbage collection enabled (plan §7.3): periodic GC on
        /// a short interval, no grace delay for unreferenced blobs (`gcDelay: 0`
        /// ⇒ collect immediately), and a retention policy that prunes untagged
        /// manifests. References (tags) keep a manifest reachable, so the lease
        /// tags pushed by [`OciStore::pin`] are honored by GC.
        fn start_gc(zot_bin: &Path) -> Zot {
            Self::spawn(zot_bin, |store, port| {
                format!(
                    r#"{{"storage":{{"rootDirectory":"{store}","dedupe":true,
                          "gc":true,"gcDelay":"0s","gcInterval":"1s",
                          "retention":{{"policies":[
                            {{"repositories":["**"],"deleteUntagged":true}}]}}}},
                        "http":{{"address":"127.0.0.1","port":"{port}"}},
                        "log":{{"level":"error"}}}}"#,
                    store = store.display(),
                )
            })
        }

        /// Spawn a child Zot whose config is built by `config` from the temp
        /// store path and a free port; wait until its API is up.
        fn spawn(zot_bin: &Path, config: impl FnOnce(&Path, u16) -> String) -> Zot {
            let dir = tempfile::tempdir().unwrap();
            let store = dir.path().join("store");
            let port = free_port();

            let config_path = dir.path().join("config.json");
            std::fs::write(&config_path, config(&store, port)).unwrap();

            let child = Command::new(zot_bin)
                .arg("serve")
                .arg(&config_path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn zot");

            let zot = Zot {
                child,
                port,
                store,
                _dir: dir,
            };
            zot.wait_ready();
            zot
        }

        fn wait_ready(&self) {
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                if let Ok(mut stream) =
                    TcpStream::connect(("127.0.0.1", self.port)).and_then(|mut s| {
                        // A bare `GET /v2/` is enough to confirm the API is up.
                        use std::io::Write;
                        s.write_all(b"GET /v2/ HTTP/1.0\r\n\r\n")?;
                        Ok(s)
                    })
                {
                    let mut buf = [0u8; 16];
                    if stream.read(&mut buf).map(|n| n > 0).unwrap_or(false) {
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(150));
            }
            panic!("zot on port {} did not become ready", self.port);
        }
    }

    /// Push the fixture layout into `zot` under `REPO` via skopeo.
    fn skopeo_push(skopeo: &Path, fixture: &Path, zot: &Zot) {
        let status = Command::new(skopeo)
            // No /etc/containers/policy.json in the Nix sandbox — accept any.
            .arg("--insecure-policy")
            .arg("copy")
            .arg("--dest-tls-verify=false")
            .arg(format!("oci:{}", fixture.display()))
            .arg(format!("docker://{}/{REPO}:latest", zot.authority()))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run skopeo");
        assert!(status.success(), "skopeo copy into zot failed");
    }

    /// Manifest digest of the fixture, read from its layout index.
    fn fixture_manifest_digest(fixture: &Path) -> Digest {
        let index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(fixture.join("index.json")).unwrap()).unwrap();
        index["manifests"][0]["digest"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        h.finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    }

    /// ensure_present caches the closure and every blob is readable from the
    /// daemon's store, byte-for-byte matching its digest; the manifest reparses
    /// as a Treadmill image.
    #[tokio::test]
    async fn ensure_present_and_verify_blobs() {
        let r = skip_unless_available!();
        let zot = Zot::start(&r.zot, None);
        skopeo_push(&r.skopeo, &r.fixture, &zot);

        let store = OciStore::new(zot.authority(), &zot.store);
        let digest = fixture_manifest_digest(&r.fixture);

        let repo = store
            .ensure_present(&digest, &[Location::new(REPO)])
            .await
            .expect("ensure_present");
        assert_eq!(repo, REPO);

        // The manifest reparses as a valid Treadmill image...
        let manifest = store.manifest(&repo, &digest).await.expect("manifest");
        let image = treadmill_rs::image::parse::parse_image(&manifest)
            .expect("manifest is a Treadmill image");
        assert_eq!(image.layers.len(), 2);

        // ...and every layer blob is present and content-addressed correctly.
        for layer in &image.layers {
            let path = store.blob_path(&repo, &layer.digest);
            assert!(path.is_file(), "layer {} missing at {path:?}", layer.digest);
            let bytes = store.read_blob(&repo, &layer.digest).await.unwrap();
            assert_eq!(
                sha256_hex(&bytes),
                layer.digest.hex(),
                "blob {} content does not match its digest",
                layer.digest,
            );
        }
    }

    /// A per-server cache pulls the image through from a canonical upstream on
    /// first ensure_present, landing the blobs in its own store.
    #[tokio::test]
    async fn pull_through_caches_from_upstream() {
        let r = skip_unless_available!();

        let canonical = Zot::start(&r.zot, None);
        skopeo_push(&r.skopeo, &r.fixture, &canonical);

        let cache = Zot::start(&r.zot, Some(canonical.port));
        let store = OciStore::new(cache.authority(), &cache.store);
        let digest = fixture_manifest_digest(&r.fixture);

        // The cache starts empty for this repo.
        assert!(
            !store.blob_path(REPO, &digest).is_file(),
            "cache unexpectedly already had the manifest",
        );

        let repo = store
            .ensure_present(&digest, &[Location::new(REPO)])
            .await
            .expect("pull-through ensure_present");

        // Now the whole closure lives in the cache's own store.
        let manifest = store.manifest(&repo, &digest).await.expect("manifest");
        let image = treadmill_rs::image::parse::parse_image(&manifest).unwrap();
        for layer in &image.layers {
            assert!(
                store.blob_path(&repo, &layer.digest).is_file(),
                "layer {} was not cached",
                layer.digest,
            );
        }
    }

    /// ensure_present fails over: an unavailable first location is skipped and a
    /// working second location serves the (digest-identical) image.
    #[tokio::test]
    async fn fails_over_to_working_location() {
        let r = skip_unless_available!();
        let zot = Zot::start(&r.zot, None);
        skopeo_push(&r.skopeo, &r.fixture, &zot);

        let store = OciStore::new(zot.authority(), &zot.store);
        let digest = fixture_manifest_digest(&r.fixture);

        let repo = store
            .ensure_present(
                &digest,
                &[
                    Location::new("treadmill/does-not-exist"),
                    Location::new(REPO),
                ],
            )
            .await
            .expect("ensure_present should fail over to the working location");
        assert_eq!(repo, REPO);
    }

    /// With no working location, ensure_present errors rather than hanging.
    #[tokio::test]
    async fn errors_when_no_location_serves() {
        let r = skip_unless_available!();
        let zot = Zot::start(&r.zot, None);
        // Intentionally do not push the image.

        let store = OciStore::new(zot.authority(), &zot.store);
        let digest = fixture_manifest_digest(&r.fixture);

        let err = store
            .ensure_present(&digest, &[Location::new(REPO)])
            .await
            .expect_err("ensure_present must fail when nothing serves the digest");
        assert!(err.to_string().contains(&digest.to_string()));
    }

    // ---- Phase 3: leases-as-references & GC (plan §7.3–7.4) ----

    /// A process-unique string, for victim blobs that must be distinct from the
    /// fixture's content-addressed blobs.
    fn unique() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        format!(
            "{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    /// Push a throwaway image with unique config + layer blobs into `repo` under
    /// `tag`, returning its `(config, layer)` blob digests. Used as the
    /// "unreferenced" image GC should reclaim once untagged.
    async fn push_victim(zot: &Zot, repo: &str, tag: &str) -> (Digest, Digest) {
        use oci_client::client::{Config, ImageLayer};

        let u = unique();
        let layer = ImageLayer::oci_v1(format!("victim-layer-{u}").into_bytes(), None);
        let config = Config::oci_v1(format!(r#"{{"victim":"{u}"}}"#).into_bytes(), None);
        let layer_digest: Digest = layer.sha256_digest().parse().unwrap();
        let config_digest: Digest = config.sha256_digest().parse().unwrap();

        let client = Client::new(ClientConfig {
            protocol: ClientProtocol::Http,
            ..Default::default()
        });
        let reference = Reference::with_tag(zot.authority(), repo.to_string(), tag.to_string());
        client
            .push(&reference, &[layer], config, &RegistryAuth::Anonymous, None)
            .await
            .expect("push victim image");

        (config_digest, layer_digest)
    }

    /// Delete a manifest `reference` (tag or digest) from `repo` on the daemon,
    /// tolerating a `404` (idempotent). Used to untag images for GC.
    async fn delete_ref(authority: &str, repo: &str, reference: &str) {
        let url = format!("http://{authority}/v2/{repo}/manifests/{reference}");
        let status = reqwest::Client::new()
            .delete(&url)
            .send()
            .await
            .expect("delete manifest reference")
            .status();
        assert!(
            status.is_success() || status == reqwest::StatusCode::NOT_FOUND,
            "deleting {reference} from {repo} failed: HTTP {status}",
        );
    }

    /// Poll `cond` until it holds or `secs` elapse. Zot's GC runs on a periodic
    /// scheduler with a randomized per-repo delay, so we wait rather than assume
    /// an immediate sweep.
    async fn wait_until(secs: u64, mut cond: impl FnMut() -> bool) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if cond() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("condition not met within {secs}s");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// The core §7.3 assumption, checked against the real Zot binary: a lease
    /// (an `inuse-<job>` reference) keeps a manifest's whole closure across a GC
    /// sweep, an *unreferenced* image is reclaimed in that same sweep, and once
    /// the lease is released the formerly-pinned closure becomes collectible.
    #[tokio::test]
    async fn lease_pins_against_gc() {
        let r = skip_unless_available!();
        let zot = Zot::start_gc(&r.zot);
        skopeo_push(&r.skopeo, &r.fixture, &zot);

        let store = OciStore::new(zot.authority(), &zot.store);
        let digest = fixture_manifest_digest(&r.fixture);
        let repo = store
            .ensure_present(&digest, &[Location::new(REPO)])
            .await
            .expect("ensure_present");

        // The blob closure the lease must protect: manifest + config + layers.
        let manifest = store.manifest(&repo, &digest).await.expect("manifest");
        let image = treadmill_rs::image::parse::parse_image(&manifest).expect("treadmill image");
        let config_digest: Digest = manifest.config().digest().to_string().parse().unwrap();
        let mut closure = vec![digest, config_digest];
        closure.extend(image.layers.iter().map(|l| l.digest));

        // Take an in-use lease on the manifest for a job.
        let job = "550e8400-e29b-41d4-a716-446655440000";
        store.pin(&repo, &digest, job).await.expect("pin");

        // Stage an unreferenced victim *in the same repo* (so a single GC pass
        // over the repo decides both) and untag it so it is collectible.
        let (victim_config, victim_layer) = push_victim(&zot, REPO, "victim").await;
        delete_ref(&zot.authority(), REPO, "victim").await;

        // Drop the fixture's catalog tag, so only the lease now references it.
        delete_ref(&zot.authority(), REPO, "latest").await;

        // GC must reclaim the victim's unique blobs...
        let victim_layer_path = store.blob_path(REPO, &victim_layer);
        wait_until(90, || !victim_layer_path.is_file())
            .await
            .expect("victim layer should be garbage-collected");
        assert!(
            !store.blob_path(REPO, &victim_config).is_file(),
            "victim config should be collected too",
        );

        // ...while the leased closure is fully retained across that same sweep.
        for d in &closure {
            assert!(
                store.blob_path(&repo, d).is_file(),
                "leased blob {d} was collected despite the in-use lease",
            );
        }

        // A concurrent ensure_present of the pinned digest stays consistent.
        store
            .ensure_present(&digest, &[Location::new(REPO)])
            .await
            .expect("re-ensure_present while leased");

        // Release the lease: nothing references the fixture now, so GC reclaims
        // the formerly-protected closure — proving the lease was load-bearing.
        store.unpin(&repo, job).await.expect("unpin");
        let manifest_path = store.blob_path(&repo, &digest);
        wait_until(90, || !manifest_path.is_file())
            .await
            .expect("unpinned manifest should be garbage-collected");
        for layer in &image.layers {
            assert!(
                !store.blob_path(&repo, &layer.digest).is_file(),
                "layer {} should be collected after release",
                layer.digest,
            );
        }
    }

    /// Several `ensure_present` calls for a leased digest, issued concurrently,
    /// all succeed and agree — ingest is serialized in the daemon and idempotent
    /// (content-addressed), and the lease keeps the closure intact throughout.
    #[tokio::test]
    async fn parallel_ensure_present_while_pinned() {
        use std::sync::Arc;

        let r = skip_unless_available!();
        let zot = Zot::start_gc(&r.zot);
        skopeo_push(&r.skopeo, &r.fixture, &zot);

        let store = Arc::new(OciStore::new(zot.authority(), &zot.store));
        let digest = fixture_manifest_digest(&r.fixture);
        let repo = store
            .ensure_present(&digest, &[Location::new(REPO)])
            .await
            .expect("ensure_present");

        let job = "11111111-2222-3333-4444-555555555555";
        store.pin(&repo, &digest, job).await.expect("pin");

        let mut handles = Vec::new();
        for _ in 0..6 {
            let store = Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                // `Digest` is `Copy`, so the `move` closure captures its own copy.
                store.ensure_present(&digest, &[Location::new(REPO)]).await
            }));
        }
        for handle in handles {
            let repo = handle
                .await
                .expect("join")
                .expect("parallel ensure_present");
            assert_eq!(repo, REPO);
        }

        // The leased closure is intact after the concurrent access.
        let manifest = store.manifest(&repo, &digest).await.expect("manifest");
        let image = treadmill_rs::image::parse::parse_image(&manifest).unwrap();
        for layer in &image.layers {
            assert!(
                store.blob_path(&repo, &layer.digest).is_file(),
                "leased layer {} missing after concurrent ensure_present",
                layer.digest,
            );
        }
    }
}

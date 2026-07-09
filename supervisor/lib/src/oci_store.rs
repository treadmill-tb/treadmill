//! Read-only client of the per-server Zot store daemon.
//!
//! Under the OCI image migration (`doc/oci-image-migration-plan.md` §6/§7) a
//! single per-server **Zot daemon owns the local store** — it is the only
//! writer. Supervisors never write its filesystem: they make a digest present
//! through the daemon's registry API and then open the daemon's on-disk blob
//! files **read-only**, pointing `qemu` / `qemu-nbd` straight at them. One
//! on-disk copy, shared page cache (D7/D8).
//!
//! Zot lays its storage out as one OCI image layout per repository:
//!
//! ```text
//! <store_root>/<repository>/blobs/sha256/<hex>
//! <store_root>/<repository>/{index.json, oci-layout}
//! ```
//!
//! Every image lives under the single local repository [`LOCAL_REPOSITORY`]:
//! upstream-supplied repository names never become local paths, and a blob
//! already fetched for any earlier image is skipped when a later image shares
//! it (`skopeo`'s per-repository blob existence check). Ingest, dedup, and GC
//! are the daemon's serialized concern; this type only drives a copy and then
//! reads files.
//!
//! "Making a digest present" copies the image from the switchboard-provided
//! upstream location into the **local** daemon by digest (a registry-to-registry
//! `skopeo copy`), landing the whole closure in the daemon's store; the
//! supervisor then opens those files read-only. Every candidate location is
//! tried in order so a supervisor fails over when one is unavailable (D16).

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use oci_spec::image::{ImageManifest, MediaType};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{Level, event, instrument};

use treadmill_rs::image::Digest;

/// The one repository all images are copied into on the local daemon.
pub const LOCAL_REPOSITORY: &str = "treadmill/images";

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
    /// the local daemon's store, trying each location in order.
    async fn ensure_present(&self, digest: &Digest, locations: &[Location]) -> Result<()>;

    /// Read and parse the (already-present) OCI image manifest `digest` from the
    /// daemon's store.
    async fn manifest(&self, digest: &Digest) -> Result<ImageManifest>;

    /// Filesystem path at which `digest`'s bytes can be opened read-only.
    fn blob_path(&self, digest: &Digest) -> PathBuf;

    /// Pin the (already-present) manifest `digest` against the daemon's garbage
    /// collector for the duration of job `job_id` (an in-use *lease*, plan
    /// §7.3). Pinning the manifest transitively protects its config and every
    /// layer blob via GC reachability. Idempotent: pinning twice is harmless.
    ///
    /// The default is a no-op, for stores with no garbage collector (e.g. test
    /// stubs); [`OciStore`] overrides it with a real Zot reference.
    async fn pin(&self, _digest: &Digest, _job_id: &str) -> Result<()> {
        Ok(())
    }

    /// Release the in-use lease pinned by [`ImageStore::pin`] for `job_id`
    /// (plan §7.3). After this the manifest is GC-eligible iff no other lease or
    /// tag references it. Idempotent: releasing an absent lease succeeds.
    ///
    /// The default is a no-op (see [`ImageStore::pin`]).
    async fn unpin(&self, _job_id: &str) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl ImageStore for OciStore {
    async fn ensure_present(&self, digest: &Digest, locations: &[Location]) -> Result<()> {
        OciStore::ensure_present(self, digest, locations).await
    }

    async fn manifest(&self, digest: &Digest) -> Result<ImageManifest> {
        OciStore::manifest(self, digest).await
    }

    fn blob_path(&self, digest: &Digest) -> PathBuf {
        OciStore::blob_path(self, digest)
    }

    async fn pin(&self, digest: &Digest, job_id: &str) -> Result<()> {
        OciStore::pin(self, digest, job_id).await
    }

    async fn unpin(&self, job_id: &str) -> Result<()> {
        OciStore::unpin(self, job_id).await
    }
}

/// One upstream the local daemon can source an image from.
///
/// The supervisor copies the image from this `(registry, repository)` upstream
/// into its *local* Zot and then reads it back off disk. Multiple locations let
/// a supervisor fail over when one is unavailable — every location serves the
/// same digest, so any that succeeds is interchangeable (D12/D16).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Location {
    /// Registry authority (`host:port`) the bytes are copied from.
    pub registry: String,
    /// Repository path within that registry, e.g. `treadmill/ubuntu-22.04`.
    pub repository: String,
}

impl Location {
    pub fn new(registry: impl Into<String>, repository: impl Into<String>) -> Self {
        Location {
            registry: registry.into(),
            repository: repository.into(),
        }
    }
}

/// A read-only client of the per-server Zot daemon.
pub struct OciStore {
    /// Registry authority of the local daemon (host:port, no scheme).
    registry: String,
    /// Filesystem root of the daemon's storage (ro from the supervisor's view).
    store_root: PathBuf,
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
        OciStore {
            registry: registry.into(),
            store_root: store_root.into(),
            http: reqwest::Client::new(),
        }
    }

    /// Path at which `digest`'s bytes can be opened (read-only). The blob is
    /// only guaranteed to exist after a successful [`OciStore::ensure_present`].
    pub fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.store_root
            .join(LOCAL_REPOSITORY)
            .join("blobs")
            .join("sha256")
            .join(digest.hex())
    }

    /// Whether `digest` is already a file in the daemon's store.
    async fn blob_on_disk(&self, digest: &Digest) -> bool {
        tokio::fs::metadata(self.blob_path(digest))
            .await
            .map(|m| m.is_file())
            .unwrap_or(false)
    }

    /// Ensure the image manifest `digest` and its whole blob closure are present
    /// in the local daemon's store, trying each location in order and failing
    /// over on error (D16).
    ///
    /// Concurrent calls for the same digest are safe: ingest is serialized inside
    /// the daemon and is idempotent (content-addressed).
    #[instrument(skip(self, locations), fields(%digest))]
    pub async fn ensure_present(&self, digest: &Digest, locations: &[Location]) -> Result<()> {
        if locations.is_empty() {
            bail!("no locations supplied to ensure_present for {digest}");
        }

        let mut failures = Vec::new();
        for location in locations {
            match self.ensure_from(location, digest).await {
                Ok(()) => {
                    event!(
                        Level::DEBUG,
                        registry = %location.registry,
                        repository = %location.repository,
                        "image present in local store",
                    );
                    return Ok(());
                }
                Err(e) => {
                    event!(
                        Level::WARN,
                        registry = %location.registry,
                        repository = %location.repository,
                        error = ?e,
                        "location failed; trying next",
                    );
                    failures.push(format!(
                        "{}/{}: {e:#}",
                        location.registry, location.repository
                    ));
                }
            }
        }

        bail!(
            "could not make {digest} present from any of {} location(s): {}",
            locations.len(),
            failures.join("; "),
        )
    }

    /// Make `digest`'s whole closure present in the local daemon by copying it
    /// from the `location` upstream into [`LOCAL_REPOSITORY`].
    async fn ensure_from(&self, location: &Location, digest: &Digest) -> Result<()> {
        if self.closure_present(digest).await? {
            return Ok(());
        }

        let source = format!(
            "docker://{}/{}@{}",
            location.registry,
            location.repository,
            digest.encoded()
        );
        let dest = format!(
            "docker://{}/{LOCAL_REPOSITORY}@{}",
            self.registry,
            digest.encoded()
        );

        let mut command = tokio::process::Command::new("skopeo");
        command
            .arg("--insecure-policy")
            .arg("copy")
            .arg("--dest-tls-verify=false");
        // Loopback upstreams (dev/test registries) are plain HTTP; everything
        // else gets mandatory TLS.
        if loopback_registry(&location.registry) {
            command.arg("--src-tls-verify=false");
        }
        let mut child = command
            .arg(&source)
            .arg(&dest)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning skopeo copy {source} -> {dest}"))?;

        if let Some(stderr) = child.stderr.take() {
            let mut lines = BufReader::new(stderr).lines();
            while let Some(line) = lines
                .next_line()
                .await
                .context("reading skopeo copy progress")?
            {
                event!(Level::DEBUG, registry = %location.registry, %digest, "skopeo copy: {line}");
            }
        }

        let status = child
            .wait()
            .await
            .with_context(|| format!("waiting for skopeo copy {source} -> {dest}"))?;
        if !status.success() {
            bail!("skopeo copy {source} -> {dest} failed: {status}");
        }

        if !self.closure_present(digest).await? {
            bail!("local store missing {digest} closure after copy");
        }

        Ok(())
    }

    /// Whether `digest`'s manifest and its entire blob closure (config + every
    /// layer) are already files in the local store.
    async fn closure_present(&self, digest: &Digest) -> Result<bool> {
        if !self.blob_on_disk(digest).await {
            return Ok(false);
        }
        let manifest = self.manifest(digest).await?;
        let descriptors = std::iter::once(manifest.config()).chain(manifest.layers().iter());
        for desc in descriptors {
            let blob: Digest = desc
                .digest()
                .to_string()
                .parse()
                .with_context(|| format!("descriptor digest {:?}", desc.digest()))?;
            if !self.blob_on_disk(&blob).await {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Read and parse the (already-present) image manifest `digest` from the
    /// daemon's store.
    #[instrument(skip(self), fields(%digest))]
    pub async fn manifest(&self, digest: &Digest) -> Result<ImageManifest> {
        let path = self.blob_path(digest);
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("reading manifest {digest} at {}", path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing manifest {digest} as an OCI image manifest"))
    }

    /// Read a blob's bytes directly from the daemon's store. Intended for small
    /// blobs (manifests, configs) and tests; large disk blobs are opened by
    /// `qemu`/`qemu-nbd` via [`OciStore::blob_path`], not read into memory here.
    pub async fn read_blob(&self, digest: &Digest) -> Result<Vec<u8>> {
        let path = self.blob_path(digest);
        tokio::fs::read(&path)
            .await
            .with_context(|| format!("reading blob {digest} at {}", path.display()))
    }

    /// The local daemon's manifest URL for `reference` (a tag or digest). The
    /// daemon is reached over loopback HTTP (`new` fixes the protocol to HTTP);
    /// token-gating is D11/Phase 5.
    fn manifest_url(&self, reference: &str) -> String {
        format!(
            "http://{}/v2/{LOCAL_REPOSITORY}/manifests/{}",
            self.registry, reference
        )
    }

    /// Pin the (already-present) manifest `digest` against the daemon's GC for
    /// the duration of job `job_id` — the in-use *lease* of plan §7.3.
    ///
    /// Implemented as a Zot **reference**: we push the manifest's own bytes back
    /// under a per-job [`inuse_tag`]. The bytes are content-addressed, so the
    /// daemon recognises the identical digest and only records a new tag
    /// pointing at the existing manifest — **no blob is copied**. Because the
    /// manifest is now tagged, the daemon's GC keeps it in `index.json`, which
    /// transitively retains its config and every layer blob (GC reachability).
    /// Idempotent: re-pinning re-PUTs the same tag.
    #[instrument(skip(self), fields(%digest))]
    pub async fn pin(&self, digest: &Digest, job_id: &str) -> Result<()> {
        // The on-disk manifest blob *is* the canonical bytes whose sha256 is
        // `digest`; re-pushing them keeps the daemon's digest identical, so the
        // tag attaches to the existing manifest rather than ingesting a copy.
        let bytes = self
            .read_blob(digest)
            .await
            .with_context(|| format!("reading manifest {digest} to pin it"))?;

        // PUT must echo the manifest's own media type; fall back to the OCI
        // image-manifest type if the blob somehow lacks the field.
        let media_type = serde_json::from_slice::<MediaTypeProbe>(&bytes)
            .ok()
            .and_then(|p| p.media_type)
            .unwrap_or_else(|| MediaType::ImageManifest.to_string());

        let tag = inuse_tag(job_id);
        let url = self.manifest_url(&tag);
        let resp = self
            .http
            .put(&url)
            .header(reqwest::header::CONTENT_TYPE, media_type)
            .body(bytes)
            .send()
            .await
            .with_context(|| format!("pinning {digest} as {tag}"))?;
        if !resp.status().is_success() {
            bail!("pinning {digest} as {tag} failed: HTTP {}", resp.status());
        }
        event!(Level::DEBUG, %tag, "pinned in-use lease");
        Ok(())
    }

    /// Release the in-use lease pinned by [`OciStore::pin`] for `job_id` (plan
    /// §7.3): delete the per-job [`inuse_tag`].
    ///
    /// Deleting the tag leaves the manifest GC-eligible only when no other
    /// reference names it (another job's lease, or a catalog tag); the daemon's
    /// GC then reclaims the now-unreferenced closure. Idempotent: a missing tag
    /// (`404`) is treated as already released.
    #[instrument(skip(self))]
    pub async fn unpin(&self, job_id: &str) -> Result<()> {
        let tag = inuse_tag(job_id);
        let url = self.manifest_url(&tag);
        let resp = self
            .http
            .delete(&url)
            .send()
            .await
            .with_context(|| format!("releasing lease {tag}"))?;
        let status = resp.status();
        if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
            event!(Level::DEBUG, %tag, "released in-use lease");
            Ok(())
        } else {
            bail!("releasing lease {tag} failed: HTTP {status}");
        }
    }
}

/// Whether a registry authority's host is loopback (`localhost`, `127.0.0.0/8`,
/// `::1`), i.e. a local plain-HTTP registry `skopeo` must not require TLS for.
fn loopback_registry(registry: &str) -> bool {
    let host = if let Some(rest) = registry.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else {
        registry.rsplit_once(':').map_or(registry, |(host, _)| host)
    };
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
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

// Integration tests that drive a real child Zot daemon (plan §12.3). They need
// `zot` and `skopeo` on `PATH` and the `tiny-efi` fixture layout in
// `TINY_EFI_IMAGE`; with any of those absent they skip, so the workspace
// `nextest` check passes them over and the dedicated `oci-store` Nix check
// (which supplies all three) runs them for real.
#[cfg(test)]
mod tests {
    use super::*;

    use oci_client::{
        Reference,
        client::{Client, ClientConfig, ClientProtocol},
        secrets::RegistryAuth,
    };

    use std::io::Read;
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
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

        /// Start Zot with a temp store.
        fn start(zot_bin: &Path) -> Zot {
            Self::spawn(zot_bin, |store, port| {
                format!(
                    r#"{{"storage":{{"rootDirectory":"{store}","dedupe":true}},
                        "http":{{"address":"127.0.0.1","port":"{port}"}},
                        "log":{{"level":"error"}}}}"#,
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

    /// Push the fixture layout into `zot` under `repo:latest` via skopeo.
    fn skopeo_push(skopeo: &Path, fixture: &Path, zot: &Zot, repo: &str) {
        let status = Command::new(skopeo)
            // No /etc/containers/policy.json in the Nix sandbox — accept any.
            .arg("--insecure-policy")
            .arg("copy")
            .arg("--dest-tls-verify=false")
            .arg(format!("oci:{}", fixture.display()))
            .arg(format!("docker://{}/{repo}:latest", zot.authority()))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run skopeo");
        assert!(status.success(), "skopeo copy into zot failed: {status:?}",);
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

    /// ensure_present copies the closure into [`LOCAL_REPOSITORY`] and every
    /// blob is readable from the daemon's store, byte-for-byte matching its
    /// digest; the manifest reparses as a Treadmill image.
    #[tokio::test]
    async fn ensure_present_and_verify_blobs() {
        let r = skip_unless_available!();
        let zot = Zot::start(&r.zot);
        skopeo_push(&r.skopeo, &r.fixture, &zot, REPO);

        let store = OciStore::new(zot.authority(), &zot.store);
        let digest = fixture_manifest_digest(&r.fixture);

        store
            .ensure_present(&digest, &[Location::new(zot.authority(), REPO)])
            .await
            .expect("ensure_present");

        // The manifest reparses as a valid Treadmill image...
        let manifest = store.manifest(&digest).await.expect("manifest");
        let image = treadmill_rs::image::parse::parse_image(&manifest)
            .expect("manifest is a Treadmill image");
        assert_eq!(image.layers.len(), 2);

        // ...and every layer blob is present and content-addressed correctly.
        for layer in &image.layers {
            let path = store.blob_path(&layer.digest);
            assert!(path.is_file(), "layer {} missing at {path:?}", layer.digest);
            let bytes = store.read_blob(&layer.digest).await.unwrap();
            assert_eq!(
                sha256_hex(&bytes),
                layer.digest.hex(),
                "blob {} content does not match its digest",
                layer.digest,
            );
        }
    }

    /// A per-server store copies the image from a canonical upstream on first
    /// ensure_present, landing the blobs in its own store.
    #[tokio::test]
    async fn copies_from_upstream() {
        let r = skip_unless_available!();

        let canonical = Zot::start(&r.zot);
        skopeo_push(&r.skopeo, &r.fixture, &canonical, REPO);

        let local = Zot::start(&r.zot);
        let store = OciStore::new(local.authority(), &local.store);
        let digest = fixture_manifest_digest(&r.fixture);

        assert!(
            !store.blob_path(&digest).is_file(),
            "local store unexpectedly already had the manifest",
        );

        store
            .ensure_present(&digest, &[Location::new(canonical.authority(), REPO)])
            .await
            .expect("ensure_present");

        // Now the whole closure lives in the local store.
        let manifest = store.manifest(&digest).await.expect("manifest");
        let image = treadmill_rs::image::parse::parse_image(&manifest).unwrap();
        for layer in &image.layers {
            assert!(
                store.blob_path(&layer.digest).is_file(),
                "layer {} was not copied",
                layer.digest,
            );
        }
    }

    /// ensure_present fails over: an unavailable first location is skipped and a
    /// working second location serves the (digest-identical) image.
    #[tokio::test]
    async fn fails_over_to_working_location() {
        let r = skip_unless_available!();
        let zot = Zot::start(&r.zot);
        skopeo_push(&r.skopeo, &r.fixture, &zot, REPO);

        let store = OciStore::new(zot.authority(), &zot.store);
        let digest = fixture_manifest_digest(&r.fixture);

        store
            .ensure_present(
                &digest,
                &[
                    Location::new(zot.authority(), "treadmill/does-not-exist"),
                    Location::new(zot.authority(), REPO),
                ],
            )
            .await
            .expect("ensure_present should fail over to the working location");
        assert!(store.blob_path(&digest).is_file());
    }

    /// With no working location, ensure_present errors rather than hanging.
    #[tokio::test]
    async fn errors_when_no_location_serves() {
        let r = skip_unless_available!();
        let zot = Zot::start(&r.zot);
        // Intentionally do not push the image.

        let store = OciStore::new(zot.authority(), &zot.store);
        let digest = fixture_manifest_digest(&r.fixture);

        let err = store
            .ensure_present(&digest, &[Location::new(zot.authority(), REPO)])
            .await
            .expect_err("ensure_present must fail when nothing serves the digest");
        assert!(err.to_string().contains(&digest.to_string()));
    }

    #[test]
    fn loopback_registry_detection() {
        assert!(loopback_registry("127.0.0.1:5000"));
        assert!(loopback_registry("127.1.2.3"));
        assert!(loopback_registry("localhost:8080"));
        assert!(loopback_registry("[::1]:5000"));
        assert!(!loopback_registry("ghcr.io"));
        assert!(!loopback_registry("registry.example.com:443"));
        assert!(!loopback_registry("10.0.0.1:5000"));
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
        // Seed the local repository directly, tagged: the tag keeps the
        // manifest referenced until the lease is taken.
        skopeo_push(&r.skopeo, &r.fixture, &zot, LOCAL_REPOSITORY);

        let store = OciStore::new(zot.authority(), &zot.store);
        let digest = fixture_manifest_digest(&r.fixture);
        store
            .ensure_present(&digest, &[Location::new(zot.authority(), LOCAL_REPOSITORY)])
            .await
            .expect("ensure_present");

        // The blob closure the lease must protect: manifest + config + layers.
        let manifest = store.manifest(&digest).await.expect("manifest");
        let image = treadmill_rs::image::parse::parse_image(&manifest).expect("treadmill image");
        let config_digest: Digest = manifest.config().digest().to_string().parse().unwrap();
        let mut closure = vec![digest, config_digest];
        closure.extend(image.layers.iter().map(|l| l.digest));

        // Take an in-use lease on the manifest for a job.
        let job = "550e8400-e29b-41d4-a716-446655440000";
        store.pin(&digest, job).await.expect("pin");

        // Stage an unreferenced victim *in the same repo* (so a single GC pass
        // over the repo decides both) and untag it so it is collectible.
        let (victim_config, victim_layer) = push_victim(&zot, LOCAL_REPOSITORY, "victim").await;
        delete_ref(&zot.authority(), LOCAL_REPOSITORY, "victim").await;

        // Drop the fixture's catalog tag, so only the lease now references it.
        delete_ref(&zot.authority(), LOCAL_REPOSITORY, "latest").await;

        // GC must reclaim the victim's unique blobs...
        let victim_layer_path = store.blob_path(&victim_layer);
        wait_until(90, || !victim_layer_path.is_file())
            .await
            .expect("victim layer should be garbage-collected");
        assert!(
            !store.blob_path(&victim_config).is_file(),
            "victim config should be collected too",
        );

        // ...while the leased closure is fully retained across that same sweep.
        for d in &closure {
            assert!(
                store.blob_path(d).is_file(),
                "leased blob {d} was collected despite the in-use lease",
            );
        }

        // A concurrent ensure_present of the pinned digest stays consistent.
        store
            .ensure_present(&digest, &[Location::new(zot.authority(), LOCAL_REPOSITORY)])
            .await
            .expect("re-ensure_present while leased");

        // Release the lease: nothing references the fixture now, so GC reclaims
        // the formerly-protected closure — proving the lease was load-bearing.
        store.unpin(job).await.expect("unpin");
        let manifest_path = store.blob_path(&digest);
        wait_until(90, || !manifest_path.is_file())
            .await
            .expect("unpinned manifest should be garbage-collected");
        for layer in &image.layers {
            assert!(
                !store.blob_path(&layer.digest).is_file(),
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
        skopeo_push(&r.skopeo, &r.fixture, &zot, LOCAL_REPOSITORY);

        let store = Arc::new(OciStore::new(zot.authority(), &zot.store));
        let digest = fixture_manifest_digest(&r.fixture);
        store
            .ensure_present(&digest, &[Location::new(zot.authority(), LOCAL_REPOSITORY)])
            .await
            .expect("ensure_present");

        let job = "11111111-2222-3333-4444-555555555555";
        store.pin(&digest, job).await.expect("pin");

        let authority = zot.authority();
        let mut handles = Vec::new();
        for _ in 0..6 {
            let store = Arc::clone(&store);
            let authority = authority.clone();
            handles.push(tokio::spawn(async move {
                // `Digest` is `Copy`, so the `move` closure captures its own copy.
                store
                    .ensure_present(&digest, &[Location::new(authority, LOCAL_REPOSITORY)])
                    .await
            }));
        }
        for handle in handles {
            handle
                .await
                .expect("join")
                .expect("parallel ensure_present");
        }

        // The leased closure is intact after the concurrent access.
        let manifest = store.manifest(&digest).await.expect("manifest");
        let image = treadmill_rs::image::parse::parse_image(&manifest).unwrap();
        for layer in &image.layers {
            assert!(
                store.blob_path(&layer.digest).is_file(),
                "leased layer {} missing after concurrent ensure_present",
                layer.digest,
            );
        }
    }
}

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
use oci_client::{
    Reference,
    client::{Client, ClientConfig, ClientProtocol},
    secrets::RegistryAuth,
};
use oci_spec::image::ImageManifest;
use tracing::{Level, event, instrument};

use treadmill_rs::image::Digest;

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
            let dir = tempfile::tempdir().unwrap();
            let store = dir.path().join("store");
            let port = free_port();

            let sync = match sync_from {
                Some(up) => format!(
                    r#","extensions":{{"sync":{{"registries":[
                        {{"urls":["http://127.0.0.1:{up}"],"onDemand":true,
                          "tlsVerify":false,"content":[{{"prefix":"**"}}]}}]}}}}"#
                ),
                None => String::new(),
            };
            let config = format!(
                r#"{{"storage":{{"rootDirectory":"{store}","dedupe":true}},
                    "http":{{"address":"127.0.0.1","port":"{port}"}},
                    "log":{{"level":"error"}}{sync}}}"#,
                store = store.display(),
            );
            let config_path = dir.path().join("config.json");
            std::fs::write(&config_path, config).unwrap();

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
}

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use tracing::{Level, event, instrument};

use treadmill_rs::image::manifest::{ImageId, ImageManifest};

const DIGEST_BYTES_NEST_LEVEL: usize = 3;

/// The image-store operations a supervisor's job state machine depends on.
///
/// Injectable (the supervisors hold `Arc<dyn ImageStore>`) so the state machine
/// can be driven by tests with a stub store, and so Phase 1 can swap the
/// TOML-backed [`LocalImageStoreClient`] for an OCI-backed implementation
/// without touching the supervisors. See `doc/oci-image-migration-plan.md` §6.
#[async_trait]
pub trait ImageStore: std::fmt::Debug + Send + Sync {
    /// Ensure the image is present locally, fetching it if necessary.
    async fn fetch_image(
        &self,
        remote_store_endpoints: Vec<String>,
        image_id: ImageId,
    ) -> Result<FetchImageStatus>;

    /// Retrieve the (already-present) image's manifest.
    async fn image_manifest(&self, image_id: ImageId) -> Result<ImageManifest>;

    /// Filesystem path at which a blob's bytes can be opened directly.
    async fn blob_path(&self, blob_sha256_digest: &[u8; 32]) -> PathBuf;
}

#[derive(Debug, Clone, Deserialize)]
pub struct LocalImageStoreConfig {
    pub fs_endpoint: PathBuf,
    pub http_endpoint: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FetchImageStatus {
    /// The image is currently being fetched. An image store may provide an
    /// optional message indicating the status of this operation.
    InProgress(Option<String>),
    /// The image has been fetched successfully, or is already present at this
    /// image store. It is available for use via the other API endpoints.
    Present,
}

#[derive(Debug)]
pub struct ImageStoreClient {
    _http_endpoint: String,
}

pub fn digest_tree_path<P: AsRef<Path>>(base: P, digest: &[u8], nest_level: usize) -> PathBuf {
    let mut current_path: PathBuf = base.as_ref().into();

    // Digest path nested into a `nest_level` deep subdirectory
    // hiearchy built from its first bytes.
    for (_level, byte) in (0..nest_level).zip(digest.iter()) {
        current_path =
            current_path.join(treadmill_rs::util::hex_slice::HexSlice(&[*byte]).to_string());
    }

    // Finally, add the full digest:
    current_path.join(treadmill_rs::util::hex_slice::HexSlice(digest).to_string())
}

impl ImageStoreClient {
    pub async fn new(http_endpoint: String) -> Result<Self> {
        Ok(ImageStoreClient {
            _http_endpoint: http_endpoint,
        })
    }

    pub async fn into_local<I: Into<PathBuf>>(
        self,
        fs_endpoint: I,
    ) -> Result<LocalImageStoreClient, (anyhow::Error, Self)> {
        LocalImageStoreClient::new(self, fs_endpoint).await
    }

    // TODO: authentication?
    #[instrument(skip(self))]
    pub async fn fetch_image(
        &self,
        _remote_store_endpoints: Vec<String>,
        _image_id: ImageId,
    ) -> Result<FetchImageStatus> {
        event!(
            Level::WARN,
            "Image store client was instructed to fetch an image, which is \
	     currently unimplemented. Returning an unconditional \
	     FetchImageStatus::Present."
        );

        Ok(FetchImageStatus::Present)
    }

    #[instrument(skip(self))]
    pub async fn image_manifest(&self, _image_id: ImageId) -> Result<ImageManifest> {
        bail!("Fetching image manifests via HTTP is not implement.")
    }
}

#[derive(Debug)]
pub struct LocalImageStoreClient {
    image_store_client: ImageStoreClient,
    fs_endpoint: PathBuf,
}

impl LocalImageStoreClient {
    pub async fn new<I: Into<PathBuf>>(
        image_store_client: ImageStoreClient,
        fs_endpoint: I,
    ) -> Result<Self, (anyhow::Error, ImageStoreClient)> {
        Ok(LocalImageStoreClient {
            image_store_client,
            fs_endpoint: fs_endpoint.into(),
        })
    }

    pub fn into_inner(self) -> ImageStoreClient {
        self.image_store_client
    }

    pub fn inner(&self) -> &ImageStoreClient {
        &self.image_store_client
    }

    pub fn inner_mut(&mut self) -> &mut ImageStoreClient {
        &mut self.image_store_client
    }

    pub async fn blob_path(&self, blob_sha256_digest: &[u8; 32]) -> PathBuf {
        digest_tree_path(
            self.fs_endpoint.join("blobs"),
            blob_sha256_digest,
            DIGEST_BYTES_NEST_LEVEL,
        )
    }

    pub async fn image_path(&self, image_id: ImageId) -> PathBuf {
        digest_tree_path(
            self.fs_endpoint.join("images"),
            &image_id.0,
            DIGEST_BYTES_NEST_LEVEL,
        )
    }

    #[instrument(skip(self))]
    pub async fn fetch_image(
        &self,
        remote_store_endpoints: Vec<String>,
        image_id: ImageId,
    ) -> Result<FetchImageStatus> {
        self.image_store_client
            .fetch_image(remote_store_endpoints, image_id)
            .await
    }

    #[instrument(skip(self))]
    pub async fn image_manifest(&self, image_id: ImageId) -> Result<ImageManifest> {
        // Only do a local lookup here. If the image store holds the image and
        // has a filesystem endpoint, it should also expose it there.
        let manifest_path = self.image_path(image_id).await;

        let manifest_bytes = tokio::fs::read(&manifest_path).await.with_context(|| {
            format!("Reading manifest of image {image_id:?} at {manifest_path:?}")
        })?;

        std::str::from_utf8(&manifest_bytes)
            .with_context(|| {
                format!(
                    "Interpreting manifest of image {image_id:?} as UTF-8 string"
                )
            })
            .and_then(|manifest_str| -> Result<ImageManifest> {
                toml::from_str(manifest_str).with_context(|| {
                    format!(
                        "Parsing manifest of image {image_id:?} as TOML-encoded ImageManifest object"
                    )
                })
            })
    }
}

#[async_trait]
impl ImageStore for LocalImageStoreClient {
    async fn fetch_image(
        &self,
        remote_store_endpoints: Vec<String>,
        image_id: ImageId,
    ) -> Result<FetchImageStatus> {
        // Inherent method (inherent resolution wins over the trait, so this is
        // not a recursive call):
        LocalImageStoreClient::fetch_image(self, remote_store_endpoints, image_id).await
    }

    async fn image_manifest(&self, image_id: ImageId) -> Result<ImageManifest> {
        LocalImageStoreClient::image_manifest(self, image_id).await
    }

    async fn blob_path(&self, blob_sha256_digest: &[u8; 32]) -> PathBuf {
        LocalImageStoreClient::blob_path(self, blob_sha256_digest).await
    }
}

use std::path::PathBuf;

use anyhow::{bail, Result};
use log::warn;

use treadmill_rs::image::manifest::{ImageId, ImageManifest};

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
    pub async fn fetch_image(
        &self,
        _remote_store_endpoints: Vec<String>,
        _image_id: ImageId,
    ) -> Result<FetchImageStatus> {
        warn!(
            "Image store client was instructed to fetch an image, which is \
	     currently unimplemented. Returning an unconditional \
	     FetchImageStatus::Present."
        );

        Ok(FetchImageStatus::Present)
    }

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

    pub async fn part_path(&self, _part_sha256_digest: &[u8; 32]) -> PathBuf {
	unimplemented!()
    }

    pub async fn fetch_image(
        &self,
        remote_store_endpoints: Vec<String>,
        image_id: ImageId,
    ) -> Result<FetchImageStatus> {
        self.image_store_client
            .fetch_image(remote_store_endpoints, image_id)
            .await
    }

    pub async fn image_manifest(&self, _image_id: ImageId) -> Result<ImageManifest> {
        // Only do a local lookup here. If the image store holds the image and
        // has a filesystem endpoint, it should also expose it there.
        // unimplemented!()
        Ok(ImageManifest {
            label: "".to_string(),
            revision: 0,
            description: "".to_string(),
	    attrs: std::collections::HashMap::new(),
            parts: std::collections::HashMap::new(),
        })
    }
}

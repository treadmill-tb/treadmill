//! Fetching OCI manifests/indexes by digest for catalog registration.
//!
//! The switchboard catalog (see `doc/oci-image-migration-plan.md` §8.1) records
//! only *references* to images — it never stores image bytes. To register an
//! image or image group it must, however, pull the manifest/index **by digest**
//! from the user's registry to validate that it is a well-formed Treadmill
//! artifact before recording a row.
//!
//! That pull is abstracted behind [`RegistryClient`] so route handlers can be
//! exercised against a canned in-memory registry in `#[sqlx::test]`s (which have
//! no Zot), while production talks the OCI Distribution protocol to a real
//! registry via [`OciRegistryClient`].

use async_trait::async_trait;
use oci_client::Reference;
use oci_client::client::{Client, ClientConfig, ClientProtocol};
use oci_client::manifest::{OCI_IMAGE_INDEX_MEDIA_TYPE, OCI_IMAGE_MEDIA_TYPE};
use oci_client::secrets::RegistryAuth;

use treadmill_rs::image::Digest;

/// Why fetching a manifest from a registry failed.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// The registry rejected the request or could not be reached.
    #[error("failed to pull {reference}: {source}")]
    Pull {
        reference: String,
        #[source]
        source: oci_client::errors::OciDistributionError,
    },
}

/// Pulls raw manifest/index bytes addressed by digest from a registry.
#[async_trait]
pub trait RegistryClient: Send + Sync {
    /// Fetch the raw manifest (or index) bytes for `digest` in
    /// `registry/repository`. The returned bytes are the exact stored document,
    /// suitable for deserializing as an `oci_spec` `ImageManifest`/`ImageIndex`.
    async fn fetch_manifest(
        &self,
        registry: &str,
        repository: &str,
        digest: &Digest,
    ) -> Result<Vec<u8>, RegistryError>;
}

/// The production [`RegistryClient`]: talks OCI Distribution to a real registry.
///
/// Pull is anonymous for now (token-gating is D11/Phase 5, matching the
/// supervisor's anonymous pull). Loopback registries are reached over plain HTTP
/// (the dev Zot has no TLS); everything else requires HTTPS.
#[derive(Default)]
pub struct OciRegistryClient;

impl OciRegistryClient {
    pub fn new() -> Self {
        OciRegistryClient
    }

    fn client_for(registry: &str) -> Client {
        // Strip the port to decide whether the authority is loopback.
        let host = registry.rsplit_once(':').map_or(registry, |(h, _)| h);
        let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]");
        let protocol = if loopback {
            ClientProtocol::Http
        } else {
            ClientProtocol::Https
        };
        Client::new(ClientConfig {
            protocol,
            ..Default::default()
        })
    }
}

#[async_trait]
impl RegistryClient for OciRegistryClient {
    async fn fetch_manifest(
        &self,
        registry: &str,
        repository: &str,
        digest: &Digest,
    ) -> Result<Vec<u8>, RegistryError> {
        let reference = Reference::with_digest(
            registry.to_string(),
            repository.to_string(),
            digest.encoded(),
        );
        let client = Self::client_for(registry);
        let (bytes, _resolved) = client
            .pull_manifest_raw(
                &reference,
                &RegistryAuth::Anonymous,
                &[OCI_IMAGE_MEDIA_TYPE, OCI_IMAGE_INDEX_MEDIA_TYPE],
            )
            .await
            .map_err(|source| RegistryError::Pull {
                reference: reference.to_string(),
                source,
            })?;
        Ok(bytes.to_vec())
    }
}

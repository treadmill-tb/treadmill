//! Legacy content-addressed image identifier.
//!
//! The home-grown TOML image manifest format has been replaced by OCI images
//! (see `doc/oci-image-migration-plan.md`); its manifest/blob-spec/extension
//! machinery is gone. Only [`ImageId`] survives, because the **client** API
//! ([`crate::api::switchboard::JobInitSpec`]) and switchboard's job table still
//! identify a requested image by the SHA-256 of its (legacy) manifest until the
//! switchboard catalog migration (Phase 4). The supervisor protocol is fully
//! content-addressed via [`crate::image::Digest`] and no longer uses this type.

use serde_with::serde_as;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Images are content-addressed by the SHA-256 checksum of their manifest.
///
/// This type can be formatted into a lower-case hex string.
#[serde_as]
#[derive(schemars::JsonSchema, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImageId(
    #[serde_as(as = "serde_with::hex::Hex")]
    #[schemars(with = "String")]
    pub [u8; 32],
);

impl ImageId {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0[..]
    }
}

impl fmt::Debug for ImageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ImageId")
            .field(&crate::util::hex_slice::HexSlice(&self.0))
            .finish()
    }
}

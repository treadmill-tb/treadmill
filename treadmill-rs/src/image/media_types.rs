//! Media types and artifact types for Treadmill OCI images.
//!
//! A Treadmill image is an OCI image manifest in *pure-artifact* form (empty
//! config + an `artifactType`). Each layer's `mediaType` names the format of
//! its blob, and nothing else: what a layer is used for is its role (see
//! [`super::annotations::ROLE`]). (Image *sets* are not OCI artifacts; they are
//! mutable, generationed switchboard entities.)

/// `artifactType` of a Treadmill image manifest (pure artifact, empty config).
pub const IMAGE_ARTIFACT_TYPE: &str = "application/vnd.treadmill.image.v2+json";

/// Prefix every version of the Treadmill image `artifactType` shares, to tell
/// an image of an unsupported version apart from something else entirely.
pub const IMAGE_ARTIFACT_TYPE_PREFIX: &str = "application/vnd.treadmill.image.";

/// A qcow2 blob. May back onto another layer (see
/// [`super::annotations::QCOW2_LOWER`]).
pub const QCOW2: &str = "application/vnd.treadmill.qcow2";

/// An uncompressed raw blob, whose size is its content's size.
pub const RAW: &str = "application/vnd.treadmill.raw";

/// Standard OCI media types that Treadmill manifests reference directly.
pub mod oci {
    /// OCI image manifest.
    pub const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
    /// The canonical empty config descriptor (`{}`), marking a pure artifact.
    pub const EMPTY_CONFIG: &str = "application/vnd.oci.empty.v1+json";
}

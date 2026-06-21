//! Media types and artifact types for Treadmill OCI images.
//!
//! A Treadmill image is an OCI image manifest in *pure-artifact* form (empty
//! config + an `artifactType`). Individual blobs carry Treadmill-specific media
//! types describing their role in a backing chain. See
//! `doc/oci-image-migration-plan.md` §D1/§D2/§D4. (Image *groups* are not OCI
//! artifacts; they are mutable, generationed switchboard entities.)

/// `artifactType` of a Treadmill image manifest (pure artifact, empty config).
pub const IMAGE_ARTIFACT_TYPE: &str = "application/vnd.treadmill.image.v1+json";

/// Media type of a qcow2 disk blob (one layer of a backing chain).
pub const DISK_QCOW2: &str = "application/vnd.treadmill.disk.qcow2";

/// Media type of a netboot boot filesystem blob (FAT image).
pub const BOOT_FAT_V1: &str = "application/vnd.treadmill.boot.fat.v1";

/// Standard OCI media types that Treadmill manifests reference directly.
pub mod oci {
    /// OCI image manifest.
    pub const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
    /// The canonical empty config descriptor (`{}`), marking a pure artifact.
    pub const EMPTY_CONFIG: &str = "application/vnd.oci.empty.v1+json";
}

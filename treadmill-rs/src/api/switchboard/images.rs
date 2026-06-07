//! Shared request/response types for the image-catalog REST API.
//!
//! These mirror the switchboard's `images` / `image-groups` routes (see
//! `doc/oci-image-migration-plan.md` §8.1). The catalog stores only references:
//! a content-addressed digest plus the `{registry, repository}` locations that
//! serve it — never image bytes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::image::Digest;

/// `POST /images`: register a concrete image by digest. The switchboard pulls
/// the manifest from `registry/repository@manifest_digest`, validates it is a
/// Treadmill image, and records an `images` row plus this first location.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct RegisterImageRequest {
    /// Registry authority (`host:port`) the manifest can be pulled from.
    pub registry: String,
    /// Repository path within the registry.
    pub repository: String,
    /// The OCI manifest digest identifying the image.
    pub manifest_digest: Digest,
    /// Optional human-readable label.
    #[serde(default)]
    pub label: Option<String>,
}

/// One registry location of a registered image.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct ImageLocation {
    pub registry: String,
    pub repository: String,
    /// `external`, `canonical`, or `system`.
    pub status: String,
}

/// A registered image, as returned by the catalog list/inspect routes.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct ImageInfo {
    pub id: Uuid,
    pub manifest_digest: Digest,
    pub artifact_type: String,
    pub owner_id: Option<Uuid>,
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
    pub locations: Vec<ImageLocation>,
}

/// `POST /image-groups`: register an image group by its OCI index digest. The
/// switchboard pulls the index, validates each member, and records the group
/// plus its (image, location, denormalized-member) rows for the matcher.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct RegisterImageGroupRequest {
    /// Registry authority (`host:port`) the index can be pulled from.
    pub registry: String,
    /// Repository path within the registry (members live in the same repo).
    pub repository: String,
    /// The OCI index digest identifying the group.
    pub index_digest: Digest,
    /// Optional human-readable label.
    #[serde(default)]
    pub label: Option<String>,
}

/// One selectable member of a registered group.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct ImageGroupMember {
    pub manifest_digest: Digest,
    pub arch: String,
    pub os: String,
    pub variant: Option<String>,
    pub target: Option<String>,
    pub board: Option<String>,
}

/// A registered image group, as returned by the catalog list/inspect routes.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct ImageGroupInfo {
    pub id: Uuid,
    pub index_digest: Digest,
    pub owner_id: Option<Uuid>,
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
    pub members: Vec<ImageGroupMember>,
}

//! Shared request/response types for the image-catalog REST API.
//!
//! These mirror the switchboard's `images` / `image-groups` routes (see
//! `doc/oci-image-migration-plan.md` §8.1). The catalog stores only references:
//! a content-addressed digest plus the `{registry, repository}` sources that
//! serve it — never image bytes. An image is a non-owned manifest identity; the
//! *sources* behind it are the ownable, grantable entity.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::image::Digest;

/// `POST /images`: register a concrete image by digest. The switchboard pulls
/// the manifest from `registry/repository@manifest_digest`, validates it is a
/// Treadmill image, and records the image plus its first location.
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

/// A permission on an image source. A "public" (unauthenticated) source is one
/// that grants the well-known `everyone` subject `use`.
#[derive(schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageSourcePermission {
    /// May pull the image from this source (run jobs that resolve to it).
    Use,
    /// May manage the source's grants and delete it (owner holds this implicitly).
    Manage,
}

/// One registry source of a registered image — the ownable, grantable catalog
/// entity — as returned by the inspect routes. `permissions` is the *viewer's*
/// permissions on this source (like host/job permission surfacing).
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct ImageSourceInfo {
    pub id: Uuid,
    pub registry: String,
    pub repository: String,
    /// `external`, `canonical`, or `system`.
    pub status: String,
    /// The source's owner, or null if orphaned.
    pub owner_id: Option<Uuid>,
    /// The viewer's permissions on this source.
    pub permissions: Vec<ImageSourcePermission>,
}

/// A registered image, as returned by the catalog list/inspect routes. An image
/// is non-owned; ownership lives on its `sources`.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct ImageInfo {
    pub id: Uuid,
    pub manifest_digest: Digest,
    pub artifact_type: String,
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
    pub sources: Vec<ImageSourceInfo>,
}

/// `POST /images/{digest}/sources`: add a registry source to a registered image.
/// The caller owns the source it adds.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct AddImageSourceRequest {
    /// Registry authority (`host:port`) the image can be pulled from.
    pub registry: String,
    /// Repository path within the registry.
    pub repository: String,
}

/// `POST /images/{digest}/sources/{source_id}/grants`: grant `permission` on a
/// source to a subject.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct ImageSourceGrantRequest {
    pub subject_id: Uuid,
    pub permission: ImageSourcePermission,
}

/// One grant on an image source, as returned by the list-grants route.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct ImageSourceGrantInfo {
    pub subject_id: Uuid,
    pub permission: ImageSourcePermission,
}

/// `POST /image-groups`: create an empty, named image group. The caller becomes
/// its owner; membership is added afterwards via per-generation snapshots (see
/// [`CreateGenerationRequest`]).
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct CreateImageGroupRequest {
    /// The stable, globally-unique moving-target handle a job references (by id).
    pub name: String,
    /// Optional human-readable label.
    #[serde(default)]
    pub label: Option<String>,
}

/// One member of a new generation; `index` is the member's array position in the
/// request, used as the deterministic tie-break among equally-specific members.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct GenerationMemberSpec {
    /// Image to include; must already be registered via `POST /images`.
    pub image_id: Uuid,
    /// Host tags a host must carry (as a superset) for this member to be
    /// selectable on it.
    #[serde(default)]
    pub required_host_tags: Vec<String>,
}

/// `POST /image-groups/{id}/generations`: append a new, immutable
/// full-replacement generation of a group's membership.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct CreateGenerationRequest {
    pub members: Vec<GenerationMemberSpec>,
}

/// A named, mutable image group, as returned by the catalog list/inspect routes.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct ImageGroupInfo {
    pub id: Uuid,
    pub name: String,
    pub label: Option<String>,
    pub owner_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    /// The group's latest generation number, or null if it has none yet.
    pub latest_generation: Option<u32>,
}

/// One member of a generation, as returned by the inspect route. A member is
/// admissible for a host iff the host's tags are a superset of
/// `required_host_tags`.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct GenerationMemberInfo {
    pub image_id: Uuid,
    pub manifest_digest: Digest,
    pub required_host_tags: Vec<String>,
    pub index: u32,
    /// Whether the viewer may use some source of this member image. A group grant
    /// is necessary but not sufficient: `false` means the member has no source the
    /// viewer can reach (so a job would not resolve it for this viewer).
    pub usable: bool,
    /// Whether *every* subject holding a `use` grant on the group can source this
    /// member (for a public group, that set includes the `everyone` subject). This
    /// is the owner-facing health signal: `false` flags a member some grantee
    /// cannot reach, so the group's `use` grant is unusable for them in practice.
    /// Vacuously `true` for a group with no `use` grants.
    pub usable_by_grantees: bool,
}

/// One immutable generation (membership snapshot) of an image group.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct ImageGroupGenerationInfo {
    pub group_id: Uuid,
    pub generation: u32,
    pub created_at: DateTime<Utc>,
    pub created_by: Option<Uuid>,
    pub members: Vec<GenerationMemberInfo>,
}

/// A permission on an image group.
#[derive(schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageGroupPermission {
    /// May run a job referencing the group (and so its member images).
    Use,
    /// May create generations and manage grants (owner holds this implicitly).
    Manage,
}

/// `POST /image-groups/{id}/grants`: grant `permission` on the group to a subject.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct ImageGroupGrantRequest {
    pub subject_id: Uuid,
    pub permission: ImageGroupPermission,
}

/// One grant on an image group, as returned by the list-grants route.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct ImageGroupGrantInfo {
    pub subject_id: Uuid,
    pub permission: ImageGroupPermission,
}

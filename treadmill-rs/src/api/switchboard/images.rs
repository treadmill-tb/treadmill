//! Shared request/response types for the image-catalog REST API.
//!
//! These mirror the switchboard's `images` / `image-sets` routes (see
//! `doc/oci-image-migration-plan.md` §8.1). The catalog stores only references:
//! a content-addressed digest plus the `{registry, repository}` sources that
//! serve it — never image bytes. An image is a non-owned manifest identity; the
//! *sources* behind it are the ownable, grantable entity.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::image::Digest;

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
    pub manifest_digest: Digest,
    pub artifact_type: String,
    pub title: Option<String>,
    pub created_at: DateTime<Utc>,
    pub sources: Vec<ImageSourceInfo>,
}

/// `POST /images/{digest}/sources`: add a registry source for an image,
/// registering the image on first sight. The caller owns the source it adds;
/// the image itself (its cached manifest projection) is non-owned and created
/// implicitly when its digest is first sourced.
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

/// `POST /image-sets`: create an empty, named image set. The caller becomes
/// its owner; membership is added afterwards via per-generation snapshots (see
/// [`CreateGenerationRequest`]).
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct CreateImageSetRequest {
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
    /// Image to include; must already be registered (have at least one source,
    /// `POST /images/{digest}/sources`).
    pub manifest_digest: Digest,
    /// Host tags a host must carry (as a superset) for this member to be
    /// selectable on it.
    #[serde(default)]
    pub required_host_tags: Vec<String>,
}

/// `POST /image-sets/{id}/generations`: append a new, immutable
/// full-replacement generation of a set's membership.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct CreateGenerationRequest {
    pub members: Vec<GenerationMemberSpec>,
}

/// A named, mutable image set, as returned by the catalog list/inspect routes.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct ImageSetInfo {
    pub id: Uuid,
    pub name: String,
    pub label: Option<String>,
    pub owner_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    /// The set's latest generation number, or null if it has none yet.
    pub latest_generation: Option<u32>,
}

/// One member of a generation, as returned by the inspect route. A member is
/// admissible for a host iff the host's tags are a superset of
/// `required_host_tags`.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct GenerationMemberInfo {
    pub manifest_digest: Digest,
    pub required_host_tags: Vec<String>,
    pub index: u32,
    /// Whether the viewer may use some source of this member image. A set grant
    /// is necessary but not sufficient: `false` means the member has no source the
    /// viewer can reach (so a job would not resolve it for this viewer).
    pub usable: bool,
    /// Whether *every* subject holding a `use` grant on the set can source this
    /// member (for a public set, the grantees include the `everyone` subject).
    /// This is the owner-facing health signal: `false` flags a member some grantee
    /// cannot reach, so the set's `use` grant is unusable for them in practice.
    /// Vacuously `true` for a set with no `use` grants.
    pub usable_by_grantees: bool,
}

/// One immutable generation (membership snapshot) of an image set.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct ImageSetGenerationInfo {
    pub set_id: Uuid,
    pub generation: u32,
    pub created_at: DateTime<Utc>,
    pub created_by: Option<Uuid>,
    pub members: Vec<GenerationMemberInfo>,
}

/// A permission on an image set.
#[derive(schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageSetPermission {
    /// May run a job referencing the set (and so its member images).
    Use,
    /// May create generations and manage grants (owner holds this implicitly).
    Manage,
}

/// `POST /image-sets/{id}/grants`: grant `permission` on the set to a subject.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct ImageSetGrantRequest {
    pub subject_id: Uuid,
    pub permission: ImageSetPermission,
}

/// One grant on an image set, as returned by the list-grants route.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct ImageSetGrantInfo {
    pub subject_id: Uuid,
    pub permission: ImageSetPermission,
}

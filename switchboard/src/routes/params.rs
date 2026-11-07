//! Typed path-parameter extractors.
//!
//! aide derives an OpenAPI path parameter for each *named field* of an object
//! schema, so a single-value `Path<Uuid>` produces no documented parameters.
//! Wrapping path segments in these structs (extracted via
//! `aide::axum::extract::Path`) declares the parameters in the spec. The field
//! names must match the `{...}` segments of the route templates — axum matches
//! path parameters by name at runtime, so a mismatch fails extraction (and the
//! route tests catch it).

use serde::Deserialize;
use uuid::Uuid;

/// A single `{id}` UUID segment.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct IdPath {
    /// The resource's unique identifier.
    pub id: Uuid,
}

/// The `{id}/services/{service}` segments of a job service route.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct JobServicePath {
    /// The job's unique identifier.
    pub id: Uuid,
    /// The service's name within the job, as the job announced it.
    pub service: String,
}

/// The `{provider}` segment of an OAuth login route.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ProviderPath {
    /// The login provider's name (e.g. `github`).
    pub provider: String,
}

/// The `{digest}` segment of an image route.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct DigestPath {
    /// The image's OCI manifest digest (`sha256:<hex>`).
    pub digest: String,
}

/// A user path segment: a user's id, or `me` for the caller.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(try_from = "String")]
pub(crate) enum UserRef {
    Me,
    Id(Uuid),
}

impl TryFrom<String> for UserRef {
    type Error = uuid::Error;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        if s == "me" {
            Ok(UserRef::Me)
        } else {
            s.parse().map(UserRef::Id)
        }
    }
}

impl UserRef {
    /// The user this segment names, given the caller's id.
    pub fn resolve(self, caller: Uuid) -> Uuid {
        match self {
            UserRef::Me => caller,
            UserRef::Id(id) => id,
        }
    }
}

/// A single `{id}` user segment, accepting `me`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct UserPath {
    /// The user's unique identifier, or `me` for the caller.
    #[schemars(with = "String")]
    pub id: UserRef,
}

/// The `{id}/tokens/{token_id}` segments of a user-token route.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct UserTokenPath {
    /// The user's unique identifier, or `me` for the caller.
    #[schemars(with = "String")]
    pub id: UserRef,
    /// The token's unique identifier.
    pub token_id: Uuid,
}

/// The `{id}/generations/{n}` segments of an image-set generation route.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct GenerationPath {
    /// The image set's unique identifier.
    pub id: Uuid,
    /// The generation number within the set.
    pub n: u32,
}

/// The `{id}/grants/{subject_id}/{permission}` segments of a grant route.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct GrantPath {
    /// The image set's unique identifier.
    pub id: Uuid,
    /// The subject (user or group) the grant applies to.
    pub subject_id: Uuid,
    /// The permission being revoked (`use` or `manage`).
    pub permission: String,
}

/// The `{id}/grants/{subject_id}/{permission}` segments of a host grant route.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct HostGrantPath {
    /// The host's unique identifier.
    pub id: Uuid,
    /// The subject (user or group) the grant applies to.
    pub subject_id: Uuid,
    /// The permission being revoked (`read`, `start` or `manage`).
    pub permission: String,
}

/// The `{digest}/sources/{source_id}` segments of an image-source route.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct SourcePath {
    /// The image's OCI manifest digest (`sha256:<hex>`).
    pub digest: String,
    /// The source's unique identifier.
    pub source_id: Uuid,
}

/// The `{digest}/sources/{source_id}/grants/{subject_id}/{permission}` segments
/// of an image-source grant route.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct SourceGrantPath {
    /// The image's OCI manifest digest (`sha256:<hex>`).
    pub digest: String,
    /// The source's unique identifier.
    pub source_id: Uuid,
    /// The subject (user or group) the grant applies to.
    pub subject_id: Uuid,
    /// The permission being revoked (`use` or `manage`).
    pub permission: String,
}

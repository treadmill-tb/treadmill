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

/// The `{token_id}` segment of a token route.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct TokenIdPath {
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

//! Host-scoped client API types.
//!
//! A host's description is its [`HostSpec`]; these types carry the operational
//! state around it — liveness, maintenance — and none of the supervisor
//! credentials or worker bookkeeping on the underlying row.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::host_spec::HostSpec;

/// A host as returned by `GET /hosts` and `GET /hosts/{id}`: its operational
/// state plus the admin-authored spec describing what it is.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct HostInfo {
    pub host_id: Uuid,
    pub name: String,
    /// Whether the host's supervisor has heartbeat recently enough to be
    /// considered schedulable, computed with the deployment's liveness window.
    pub live: bool,
    /// The host's last heartbeat, or null if it has never reported (or its
    /// supervisor disconnected cleanly).
    pub last_seen_at: Option<DateTime<Utc>>,
    /// Whether an operator has withheld this host from scheduling. A host in
    /// maintenance is neither dispatched onto nor preempted to free capacity.
    pub maintenance: bool,
    /// The host's current spec, normalized to the latest version. Null only for
    /// a host that has never been described.
    pub spec: Option<HostSpec>,
    /// The revision `spec` was read at, for `If-Match` on a spec write. Null
    /// exactly when `spec` is.
    pub spec_revision: Option<i32>,
}

/// A change to a host's operational state, carried by `PATCH /hosts/{id}`.
///
/// Only the fields present are changed. Host *description* is not editable
/// here: it lives in the host's spec, which is versioned separately.
#[derive(schemars::JsonSchema, Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostUpdateRequest {
    /// Withhold the host from scheduling, or return it to service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<bool>,
}

/// A new host (`POST /hosts`): the `hosts` row and revision 1 of its spec are
/// written in one transaction, so a host is never in an undescribed state.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCreateRequest {
    /// The host's spec. Its `id` becomes the host's id: the client supplies the
    /// UUID so a spec is a self-contained document that can live in a git repo
    /// and be applied. Rejected with a [`HostSpecRejection`] naming the
    /// offending field if it does not validate.
    #[schemars(with = "HostSpec")]
    pub spec: serde_json::Value,
}

/// The created host, and the credential its supervisor authenticates with.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct HostCreateResponse {
    pub host_id: Uuid,
    /// Base64 bearer token for the host's `/hosts/{id}/connect` WebSocket. The
    /// API never returns it again, so it has to be captured here.
    pub auth_token: String,
    /// The revision the spec was stored at, always the first.
    pub spec_revision: i32,
}

/// A new revision of a host's spec (`PUT /hosts/{id}/spec`).
///
/// The write is conditional on an `If-Match` header carrying the revision the
/// caller last read, so two admins editing one host cannot silently clobber
/// each other.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostSpecUpdateRequest {
    /// The replacement spec. Its `id` must be the host being written.
    #[schemars(with = "HostSpec")]
    pub spec: serde_json::Value,
}

/// The outcome of a spec write.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct HostSpecUpdateResponse {
    /// The revision the document was stored at; the next write's `If-Match`.
    pub spec_revision: i32,
}

/// Why a submitted host spec was refused (`422 Unprocessable Entity`).
///
/// Specs are hand-edited documents, so a rejection names the offending field
/// rather than a byte offset.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct HostSpecRejection {
    /// Dotted path to the offending field, e.g. `duts[0].debug.probe.serial`.
    /// Empty when the fault is the document as a whole.
    pub path: String,
    /// Human-readable explanation; not intended to be parsed.
    pub message: String,
}

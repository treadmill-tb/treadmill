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

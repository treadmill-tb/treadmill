//! Host-scoped client API types.
//!
//! These back the read-only `GET /hosts` listing, which exists so a frontend
//! (the web console's dispatch form) can populate a host picker. The listing is
//! intentionally a *view*: it exposes a host's user-facing identity, its opaque
//! tags, its attached targets (DUTs), and a liveness flag — but none of the
//! supervisor credentials or worker bookkeeping on the underlying row.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One target (DUT) attached to a host, as exposed by `GET /hosts`.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct HostTarget {
    /// Stable per-host label for the DUT (e.g. `"dut0"`).
    pub name: String,
    /// Opaque tag set (same convention as host tags).
    pub tags: Vec<String>,
}

/// A host in the `GET /hosts` listing: its identity, opaque tags, attached
/// targets, and liveness.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct HostInfo {
    pub host_id: Uuid,
    pub name: String,
    /// Opaque host tags; a job's `host_tag_requirements` match a host whose
    /// `tags` are a superset.
    pub tags: Vec<String>,
    /// Whether the host's supervisor has heartbeat recently enough to be
    /// considered schedulable, computed with the deployment's liveness window.
    pub live: bool,
    /// The host's last heartbeat, or `None` if it has never reported (or its
    /// worker disconnected cleanly).
    pub last_seen_at: Option<DateTime<Utc>>,
    /// The targets (DUTs) wired to this host, in stable order.
    pub targets: Vec<HostTarget>,
}

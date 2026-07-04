//! Shared response types for the audit-feed read API.
//!
//! These mirror the switchboard's per-entity audit feed (`GET
//! /{entity}/{id}/events`). They live here, in the shared crate, so any HTTP
//! client (the web console, an eventual CLI) deserializes the *same* structs the
//! switchboard serializes from — a shape change is a compile error on both
//! sides, with no codegen in between.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A page of rendered audit events for one entity, newest first.
///
/// Paginated by keyset: when `next_cursor` is non-null, pass it back as the
/// `cursor` query parameter to fetch the next (older) page; a null `next_cursor`
/// means the last page.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct AuditFeedResponse {
    pub events: Vec<RenderedAuditRow>,
    /// Opaque cursor for the next page, or null on the last page.
    pub next_cursor: Option<String>,
}

/// One audit event, already rendered to a human-readable `message` for the
/// requesting viewer (the switchboard resolves the per-event template and
/// applies the viewer's visibility before sending it).
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct RenderedAuditRow {
    pub event_id: Uuid,
    pub event_type: String,
    pub created_at: DateTime<Utc>,
    pub actor_id: Uuid,
    pub correlation_id: Option<Uuid>,
    pub message: String,
}

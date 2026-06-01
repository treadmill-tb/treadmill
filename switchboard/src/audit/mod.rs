//! # Audit & observability
//!
//! Switchboard separates three concerns that are often conflated. They share a
//! vocabulary (the typed event structs in this module) but NOT an emission path:
//!
//!   1. **Audit log** (this module -> Postgres). Durable, gapless, immutable,
//!      transactional, queried by entity. The record of *what the system did*.
//!      Written in the same transaction as the state change it describes.
//!
//!   2. **Operational telemetry** (`tracing` -> console/Sentry). Best-effort,
//!      sampled, ephemeral; for debugging and alerting. Carries correlation ids
//!      that are stamped onto audit rows, but is NEVER the source of truth for
//!      the audit log.
//!
//!   3. **Client-facing event feed** (read API). A product surface that renders
//!      audit rows for a viewer, filtered by that viewer's permissions on the
//!      related entities.
//!
//! Audit rows are persisted, then published to (2)/(3) only AFTER the
//! transaction commits -- a rolled-back state change never announces an event.
//!
//! See `AUDIT_LOG_PLAN.md` for the full design rationale.
//!
//! ## Module layout
//!
//! - [`model`]: the typed event vocabulary -- the [`AuditEvent`] trait plus the
//!   [`Relation`] / [`Role`] / [`ViewPolicy`] / [`EntityRef`] / [`Permission`]
//!   types every event implementation describes itself with.
//! - [`persist`]: persists an [`AuditEvent`] and its relations into the
//!   `tml_switchboard.audit_events` / `audit_event_relations` tables.
//! - [`registry`]: the link-time renderer registry the read API consults to turn
//!   a stored payload into a viewer-facing string at view time.
//!
//! Phase 3 of the plan will wrap [`persist::persist_event`] in a `transition()`
//! chokepoint guarded by a `WriteToken` so the audit emission is unforgeable
//! from outside this module; until then `persist_event` is `pub(crate)`.

pub mod model;
pub mod persist;
pub mod registry;

pub use model::{AuditEvent, EntityRef, Permission, Relation, Role, ViewPolicy};
pub use registry::{RenderedEvent, ViewerCtx, render};

/// The well-known `system` subject seeded in `SCHEMA.sql`. Worker-driven
/// transitions and other internal automation attribute their audit events to
/// this subject because no human is responsible for the action.
pub const SYSTEM_ACTOR_ID: uuid::Uuid = uuid::Uuid::from_u128(2);

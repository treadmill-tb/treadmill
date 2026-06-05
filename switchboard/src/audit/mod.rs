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

pub mod events;
pub mod feed;
pub mod macros;
pub mod model;
pub mod persist;
pub mod registry;

pub use model::{AuditEvent, EntityRef, Permission, Relation, Role, ViewPolicy};
pub use registry::{RenderedEvent, ViewerCtx, render};

/// The well-known `system` subject seeded in `SCHEMA.sql`. Worker-driven
/// transitions and other internal automation attribute their audit events to
/// this subject because no human is responsible for the action.
pub const SYSTEM_ACTOR_ID: uuid::Uuid = uuid::Uuid::from_u128(2);

use sqlx::{PgConnection, Transaction, Postgres};

/// Proof of being inside an audited transition. Its only field is private to
/// the `audit` module, so it can be constructed ONLY by `audit::transition`
/// and `audit::emit`. This token guarantees that any helper function requiring
/// it can only be called from inside the audit chokepoint.
pub struct WriteToken(());

/// A state transition that writes to the database and emits an audit event
/// describing that write.
#[allow(async_fn_in_trait)]
pub trait Transition {
    /// The value returned by the transition upon success (e.g. an inserted ID).
    type Output;
    /// The typed audit event describing the change.
    type Event: AuditEvent;

    /// Performs the mutation AND returns the event describing it.
    ///
    /// Because the event is built from the same inputs as the write, the two
    /// cannot diverge. Implementations must use `txn` for all writes and must
    /// pass `w` to any data-access helpers to prove they are running inside
    /// the chokepoint.
    async fn apply(
        self,
        txn: &mut PgConnection,
        w: &WriteToken,
    ) -> Result<(Self::Output, Self::Event), sqlx::Error>;
}

/// The enforced path for state changes.
///
/// Bundles a [`Transition`]'s write and its corresponding audit event in one
/// transaction. By requiring callers to pass `t` (the un-executed transition),
/// this function guarantees that the only way to perform the write is to also
/// generate the event, and that both are persisted atomically.
pub async fn transition<T: Transition>(
    txn: &mut Transaction<'_, Postgres>,
    t: T,
) -> Result<T::Output, sqlx::Error> {
    let w = WriteToken(());
    let (out, event) = t.apply(txn, &w).await?;
    // Pass None for correlation_id for now (Phase 8 handles tracing extraction)
    persist::persist_event(txn, &event, None).await?;
    Ok(out)
}

/// The enforced path for observational events that perform no mutation
/// (e.g. recording an access).
///
/// Takes a `&mut Transaction` so it joins the surrounding transaction,
/// guaranteeing the event is only committed if the broader operation succeeds.
pub async fn emit<E: AuditEvent>(
    txn: &mut Transaction<'_, Postgres>,
    event: &E,
) -> Result<(), sqlx::Error> {
    // Pass None for correlation_id for now (Phase 8 handles tracing extraction)
    persist::persist_event(txn, event, None).await?;
    Ok(())
}

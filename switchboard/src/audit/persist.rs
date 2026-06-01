//! Write an [`AuditEvent`] into the `tml_switchboard.audit_events` table along
//! with one `audit_event_relations` row per [`Relation`] the event declares.
//!
//! Persistence is the only operation in this module that touches the database;
//! everything else (rendering, registry lookups) is in-process. Both inserts
//! ride on the caller's transaction, so the event becomes visible iff the
//! caller commits -- a rolled-back state change leaves no audit row behind.
//! That atomicity is the whole point of the audit log living in Postgres in
//! the first place; see plan §6.
//!
//! Phase 3 will wrap this function in `audit::transition()` and require a
//! `WriteToken` to mint a row, so that no code outside the audit module can
//! emit an event without going through the chokepoint. Until then this is
//! `pub(crate)` and called only from tests.

use serde::Serialize;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::model::AuditEvent;

/// Persist a single audit event and its relation rows on the caller's
/// transaction.
///
/// Mints a fresh UUIDv7 for the row -- time-ordered so the primary key index
/// also serves as the natural newest-first ordering for the entity-scoped feed.
/// The returned id is the value written to `audit_events.event_id`.
///
/// `correlation_id` is the `tracing` trace id stamped onto the row; pass
/// `None` for events raised outside a request span. Phase 8 of the plan will
/// extract this from the current span automatically.
// Phase 3 of the plan introduces `audit::transition()` as the sole caller;
// until then this function has only test callers, so non-test builds see no
// non-test use site and would warn.
#[allow(dead_code)]
pub(crate) async fn persist_event<E: AuditEvent>(
    txn: &mut Transaction<'_, Postgres>,
    event: &E,
    correlation_id: Option<Uuid>,
) -> Result<Uuid, sqlx::Error> {
    let event_id = Uuid::now_v7();
    let event_type = event.event_type();
    let actor_id = event.actor();
    let relations = event.relations();

    // `serde_json::Value` is sqlx's natural mapping for `jsonb`. Serialization
    // is infallible for any well-behaved `Serialize` impl (the event structs
    // are plain data); a failure here would indicate a bug in the event type
    // itself, so we surface it as `Encode`.
    let payload = serde_json::to_value(event).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

    sqlx::query!(
        r#"
        insert into tml_switchboard.audit_events
            (event_id, event_type, payload, actor_id, correlation_id)
        values
            ($1, $2, $3, $4, $5)
        "#,
        event_id,
        event_type,
        payload,
        actor_id,
        correlation_id,
    )
    .execute(&mut **txn)
    .await?;

    // Most events have 1-3 relations; a per-relation insert is simpler than
    // unnest-driven bulk insert, and the row count stays trivial.
    for relation in &relations {
        let entity_kind = relation.entity.kind_str();
        let entity_id = relation.entity.id();
        let role = relation.role.as_str();
        let view_policy = relation.view.as_str();

        sqlx::query!(
            r#"
            insert into tml_switchboard.audit_event_relations
                (event_id, entity_kind, entity_id, role, view_policy)
            values
                ($1, $2::tml_switchboard.audit_entity_kind, $3, $4::tml_switchboard.audit_role, $5)
            "#,
            event_id,
            entity_kind as _,
            entity_id,
            role as _,
            view_policy,
        )
        .execute(&mut **txn)
        .await?;
    }

    Ok(event_id)
}

/// Stored shape of an audit row, returned by reader queries.
///
/// Used internally by the read API; the public viewer-facing representation
/// is produced by feeding `payload` through
/// [`registry::render`](super::registry::render).
#[derive(Debug, Clone, Serialize)]
pub struct StoredEvent {
    pub event_id: Uuid,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub actor_id: Uuid,
    pub correlation_id: Option<Uuid>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[cfg(test)]
mod tests {
    //! DB-backed tests for the audit-event persistence path. Like the other
    //! `#[sqlx::test]` suites in this crate, every test is `#[ignore]` and
    //! needs the `.#database` devshell -- see `supervisor_ws_worker::tests`
    //! for the invocation.

    use super::*;
    use crate::audit::SYSTEM_ACTOR_ID;
    use crate::audit::model::{AuditEvent, EntityRef, Permission, Relation, Role, ViewPolicy};
    use crate::auth::engine::HostPermission;
    use serde::Serialize;
    use sqlx::PgPool;

    /// Minimal event used only by these tests: one relation per role kind so
    /// the relation insert is exercised end-to-end.
    #[derive(Serialize)]
    struct DummyEvent {
        host_id: Uuid,
        subject_id: Uuid,
        note: &'static str,
    }

    impl AuditEvent for DummyEvent {
        fn event_type(&self) -> &'static str {
            "test.dummy.v1"
        }
        fn actor(&self) -> Uuid {
            SYSTEM_ACTOR_ID
        }
        fn relations(&self) -> Vec<Relation> {
            vec![
                Relation {
                    entity: EntityRef::Host(self.host_id),
                    role: Role::Subject,
                    view: ViewPolicy::Permission(Permission::from(HostPermission::Read)),
                },
                Relation {
                    entity: EntityRef::Subject(self.subject_id),
                    role: Role::Actor,
                    view: ViewPolicy::OperatorOnly,
                },
            ]
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn persist_event_writes_row_and_relations(pool: PgPool) -> anyhow::Result<()> {
        let event = DummyEvent {
            host_id: Uuid::new_v4(),
            subject_id: Uuid::new_v4(),
            note: "hello",
        };
        let correlation = Some(Uuid::new_v4());

        let mut txn = pool.begin().await?;
        let event_id = persist_event(&mut txn, &event, correlation).await?;
        txn.commit().await?;

        // The event row carries the typed fields and the raw payload.
        let stored = sqlx::query!(
            r#"
            select event_type, payload, actor_id, correlation_id
            from tml_switchboard.audit_events
            where event_id = $1
            "#,
            event_id,
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(stored.event_type, "test.dummy.v1");
        assert_eq!(stored.actor_id, SYSTEM_ACTOR_ID);
        assert_eq!(stored.correlation_id, correlation);
        assert_eq!(stored.payload, serde_json::to_value(&event)?);

        // Both declared relations landed, with the right kind/role/policy.
        let rels = sqlx::query!(
            r#"
            select
                entity_kind::text as "entity_kind!",
                entity_id,
                role::text as "role!",
                view_policy
            from tml_switchboard.audit_event_relations
            where event_id = $1
            order by role
            "#,
            event_id,
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(rels.len(), 2);

        let actor_rel = rels.iter().find(|r| r.role == "actor").unwrap();
        assert_eq!(actor_rel.entity_kind, "subject");
        assert_eq!(actor_rel.entity_id, event.subject_id);
        assert_eq!(actor_rel.view_policy, "operator_only");

        let subject_rel = rels.iter().find(|r| r.role == "subject").unwrap();
        assert_eq!(subject_rel.entity_kind, "host");
        assert_eq!(subject_rel.entity_id, event.host_id);
        assert_eq!(subject_rel.view_policy, "read");

        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn audit_events_are_append_only(pool: PgPool) -> anyhow::Result<()> {
        let event = DummyEvent {
            host_id: Uuid::new_v4(),
            subject_id: Uuid::new_v4(),
            note: "immutable",
        };

        let mut txn = pool.begin().await?;
        let event_id = persist_event(&mut txn, &event, None).await?;
        txn.commit().await?;

        // The append-only trigger rejects direct UPDATE...
        let update = sqlx::query!(
            "update tml_switchboard.audit_events set event_type = 'tampered' where event_id = $1",
            event_id,
        )
        .execute(&pool)
        .await;
        assert!(update.is_err(), "UPDATE should be rejected by trigger");

        // ...and DELETE.
        let delete = sqlx::query!(
            "delete from tml_switchboard.audit_events where event_id = $1",
            event_id,
        )
        .execute(&pool)
        .await;
        assert!(delete.is_err(), "DELETE should be rejected by trigger");

        Ok(())
    }
}

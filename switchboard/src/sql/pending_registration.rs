//! Persistence for a login that has passed admission but is awaiting Terms of
//! Service acceptance (the ToS interstitial).
//!
//! A `pending_registrations` row is short-lived, consume-once staging in the
//! same spirit as the CSRF `oauth_flows` row: it carries the derived identity
//! (for a brand-new user) or the existing user id (for a re-acceptance on a ToS
//! version bump), plus the resolved org ids -- but NEVER the OAuth access token.
//! It is deleted in the same transaction that provisions the user or records the
//! re-acceptance.

use chrono::{DateTime, Utc};
use sqlx::PgExecutor;
use uuid::Uuid;

use crate::auth::oauth::ExternalIdentity;

/// A staged, not-yet-completed registration awaiting ToS acceptance.
///
/// Exactly one of `identity` / `existing_user_id` is set (a DB CHECK enforces
/// it): `identity` for a brand-new user awaiting first acceptance,
/// `existing_user_id` for an existing user re-accepting a bumped ToS.
#[derive(Debug, Clone)]
pub struct PendingRegistration {
    pub id: Uuid,
    pub provider: String,
    /// The serialized [`ExternalIdentity`] (jsonb) for a brand-new registration,
    /// or `None` for an existing-user re-acceptance.
    pub identity: Option<serde_json::Value>,
    /// The existing user re-accepting, or `None` for a brand-new registration.
    pub existing_user_id: Option<Uuid>,
    pub org_ids: Vec<String>,
    pub expires_at: DateTime<Utc>,
}

impl PendingRegistration {
    /// Deserialize the staged [`ExternalIdentity`], if this row stages a
    /// brand-new registration. `None` for an existing-user re-acceptance.
    pub fn parse_identity(&self) -> Option<Result<ExternalIdentity, serde_json::Error>> {
        self.identity
            .clone()
            .map(serde_json::from_value::<ExternalIdentity>)
    }
}

/// Delete any expired staging rows. Called opportunistically on insert so an
/// abandoned interstitial (a user who never accepts) does not accumulate. The
/// `expires_at` filter on [`consume_pending`] guarantees correctness regardless;
/// this is purely housekeeping.
pub async fn sweep_expired(conn: impl PgExecutor<'_>) -> Result<(), sqlx::Error> {
    sqlx::query!("delete from tml_switchboard.pending_registrations where expires_at < now();")
        .execute(conn)
        .await
        .map(|_| ())
}

/// Stage a registration awaiting ToS acceptance. Provide EITHER `identity` (a
/// brand-new user) OR `existing_user_id` (a re-acceptance), never both -- the DB
/// CHECK enforces this. Sweeps expired rows first.
#[allow(clippy::too_many_arguments)]
pub async fn insert_pending(
    pool: &sqlx::PgPool,
    id: Uuid,
    provider: &str,
    identity: Option<&ExternalIdentity>,
    existing_user_id: Option<Uuid>,
    org_ids: &[String],
    expires_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sweep_expired(pool).await?;

    let identity_json = match identity {
        Some(i) => Some(serde_json::to_value(i).map_err(|e| sqlx::Error::Encode(Box::new(e)))?),
        None => None,
    };

    sqlx::query!(
        "insert into tml_switchboard.pending_registrations \
         (id, provider, identity, existing_user_id, org_ids, expires_at) \
         values ($1, $2, $3, $4, $5, $6);",
        id,
        provider,
        identity_json,
        existing_user_id,
        org_ids,
        expires_at,
    )
    .execute(pool)
    .await
    .map(|_| ())
}

/// Consume (delete) the staging row identified by `id`, returning it iff the row
/// existed and has not expired. Mirrors [`crate::sql::oauth_flow::consume_flow`]:
/// consume-once, and an expired row is still deleted (swept) but yields `None`.
///
/// Runs on the caller's executor so it can join the transaction that provisions
/// the user / records the re-acceptance, keeping the whole thing atomic.
pub async fn consume_pending(
    conn: impl PgExecutor<'_>,
    id: Uuid,
) -> Result<Option<PendingRegistration>, sqlx::Error> {
    let row = sqlx::query!(
        "delete from tml_switchboard.pending_registrations where id = $1 \
         returning id, provider, identity, existing_user_id, org_ids, expires_at;",
        id,
    )
    .fetch_optional(conn)
    .await?;

    Ok(row.and_then(|r| {
        (r.expires_at > Utc::now()).then_some(PendingRegistration {
            id: r.id,
            provider: r.provider,
            identity: r.identity,
            existing_user_id: r.existing_user_id,
            org_ids: r.org_ids,
            expires_at: r.expires_at,
        })
    }))
}

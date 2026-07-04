//! Persistence for a login that has passed admission but is awaiting Terms of
//! Service acceptance (the login-completion step).
//!
//! A `pending_registrations` row is short-lived, consume-once staging in the
//! same spirit as the CSRF `oauth_flows` row: it carries the derived identity
//! (for a brand-new user) or the existing user id (for a re-acceptance on a ToS
//! version bump), plus the resolved org ids -- but NEVER the OAuth access token.
//! It is deleted in the same transaction that provisions the user or records the
//! re-acceptance.
//!
//! The row id is NOT a capability: consuming it requires the one-time secret
//! whose salted hash (see [`crate::auth::pending_secret`]) is stored alongside.

use chrono::{DateTime, Utc};
use sqlx::{PgExecutor, Postgres, Transaction};
use uuid::Uuid;

use crate::auth::oauth::ExternalIdentity;
use crate::auth::pending_secret;

/// A staged, not-yet-completed login awaiting ToS acceptance.
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
/// abandoned completion step (a user who never accepts) does not accumulate.
/// The `expires_at` filter on [`consume_pending`] guarantees correctness
/// regardless; this is purely housekeeping.
pub async fn sweep_expired(conn: impl PgExecutor<'_>) -> Result<(), sqlx::Error> {
    sqlx::query!("delete from tml_switchboard.pending_registrations where expires_at < now();")
        .execute(conn)
        .await
        .map(|_| ())
}

/// Stage a login awaiting ToS acceptance. Provide EITHER `identity` (a
/// brand-new user) OR `existing_user_id` (a re-acceptance), never both -- the DB
/// CHECK enforces this. `secret_hash` is the salted hash of the one-time
/// completion secret (never the secret itself). Sweeps expired rows first.
#[allow(clippy::too_many_arguments)]
pub async fn insert_pending(
    pool: &sqlx::PgPool,
    id: Uuid,
    secret_hash: &str,
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
         (id, secret_hash, provider, identity, existing_user_id, org_ids, expires_at) \
         values ($1, $2, $3, $4, $5, $6, $7);",
        id,
        secret_hash,
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

/// Consume the staging row identified by `id`, returning it iff the row exists,
/// has not expired, and `secret` matches its stored hash.
///
/// Verify-then-delete, with the row locked in between: a presented secret that
/// does not match leaves the row untouched, so a caller knowing only the id can
/// neither complete nor burn someone else's staged login. Deletion (of a
/// matched or expired row) joins the caller's transaction, keeping the
/// consume-once guarantee atomic with the user provisioning / re-acceptance it
/// gates; concurrent attempts serialize on the row lock, and the loser finds
/// the row gone.
pub async fn consume_pending(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    secret: &str,
) -> Result<Option<PendingRegistration>, sqlx::Error> {
    let Some(row) = sqlx::query!(
        "select id, secret_hash, provider, identity, existing_user_id, org_ids, expires_at \
         from tml_switchboard.pending_registrations where id = $1 for update;",
        id,
    )
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(None);
    };

    if !pending_secret::verify(secret, &row.secret_hash) {
        return Ok(None);
    }

    // Secret holder confirmed: consume the row whether it is still live or
    // already expired (deleting an expired row is just the sweep, early).
    sqlx::query!(
        "delete from tml_switchboard.pending_registrations where id = $1;",
        id,
    )
    .execute(&mut **tx)
    .await?;

    Ok(
        (row.expires_at > Utc::now()).then_some(PendingRegistration {
            id: row.id,
            provider: row.provider,
            identity: row.identity,
            existing_user_id: row.existing_user_id,
            org_ids: row.org_ids,
            expires_at: row.expires_at,
        }),
    )
}

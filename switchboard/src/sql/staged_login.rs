//! Persistence for a staged login: an interactive login whose identity the
//! OAuth callback has verified (token exchanged, admission passed), awaiting
//! its completion at `POST /auth/login/complete` — the sole point that mints a
//! session token. A staged login may still require completion steps (ToS
//! acceptance), or be immediately claimable.
//!
//! A `staged_logins` row is short-lived, consume-once staging in the same
//! spirit as the CSRF `oauth_flows` row: it carries the derived identity (for a
//! brand-new user) or the existing user id, plus the resolved org ids -- but
//! NEVER the OAuth access token. It is deleted in the same transaction that
//! provisions the user or records the login.

use chrono::{DateTime, Utc};
use sqlx::{PgExecutor, Postgres, Transaction};
use uuid::Uuid;

use crate::auth::oauth::ExternalIdentity;
use crate::auth::staged_secret;

/// A staged, not-yet-completed login awaiting completion.
///
/// Exactly one of `identity` / `existing_user_id` is set (a DB CHECK enforces
/// it): `identity` for a brand-new user awaiting first acceptance,
/// `existing_user_id` for an existing user (one that requires some actions,
/// e.g. re-accepting a bumped ToS, or one whose login is ready to claim).
#[derive(Debug, Clone)]
pub struct StagedLogin {
    pub id: Uuid,
    pub provider: String,
    /// The serialized [`ExternalIdentity`] (jsonb) for a brand-new
    /// registration, or `None` for an existing user.
    pub identity: Option<serde_json::Value>,
    /// The existing user logging in, or `None` for a brand-new registration.
    pub existing_user_id: Option<Uuid>,
    pub org_ids: Vec<String>,
    /// The initiating flow's declared (allowlist-validated) browser return
    /// URL; `None` for a programmatic flow.
    pub return_to: Option<String>,
    /// Browser context captured at the OAuth callback, stamped onto the
    /// session token minted at completion.
    pub user_agent: Option<String>,
    pub created_ip: Option<String>,
    pub created_port: Option<i32>,
    pub expires_at: DateTime<Utc>,
}

impl StagedLogin {
    /// Deserialize the staged [`ExternalIdentity`], if this row stages a
    /// brand-new registration. `None` for an existing user.
    pub fn parse_identity(&self) -> Option<Result<ExternalIdentity, serde_json::Error>> {
        self.identity
            .clone()
            .map(serde_json::from_value::<ExternalIdentity>)
    }
}

/// The fields of a new staged login; see [`insert_staged`].
#[derive(Debug, Clone, Copy)]
pub struct StageLogin<'a> {
    pub id: Uuid,
    /// Salted hash of the one-time completion secret (never the secret itself).
    pub secret_hash: &'a str,
    pub provider: &'a str,
    /// EITHER a brand-new user's identity ...
    pub identity: Option<&'a ExternalIdentity>,
    /// ... OR an existing user, never both -- the DB CHECK enforces this.
    pub existing_user_id: Option<Uuid>,
    pub org_ids: &'a [String],
    pub return_to: Option<&'a str>,
    pub user_agent: Option<&'a str>,
    pub created_ip: Option<&'a str>,
    pub created_port: Option<i32>,
    pub expires_at: DateTime<Utc>,
}

/// Delete any expired staging rows. Called opportunistically when the callback
/// stages a login, so an abandoned completion step (a user who never accepts)
/// does not accumulate. The `expires_at` filter on [`consume_staged`]
/// guarantees correctness regardless; this is purely housekeeping.
pub async fn sweep_expired(conn: impl PgExecutor<'_>) -> Result<(), sqlx::Error> {
    sqlx::query!("delete from tml_switchboard.staged_logins where expires_at < now();")
        .execute(conn)
        .await
        .map(|_| ())
}

/// Stage a login. Takes any executor so a re-stage/successor row can join the
/// completion transaction that consumes its predecessor; callers on the pool
/// path pair this with [`sweep_expired`].
pub async fn insert_staged(
    conn: impl PgExecutor<'_>,
    staged: StageLogin<'_>,
) -> Result<(), sqlx::Error> {
    let identity_json = match staged.identity {
        Some(i) => Some(serde_json::to_value(i).map_err(|e| sqlx::Error::Encode(Box::new(e)))?),
        None => None,
    };

    sqlx::query!(
        "insert into tml_switchboard.staged_logins \
         (id, secret_hash, provider, identity, existing_user_id, org_ids, \
          return_to, user_agent, created_ip, created_port, expires_at) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11);",
        staged.id,
        staged.secret_hash,
        staged.provider,
        identity_json,
        staged.existing_user_id,
        staged.org_ids,
        staged.return_to,
        staged.user_agent,
        staged.created_ip,
        staged.created_port,
        staged.expires_at,
    )
    .execute(conn)
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
pub async fn consume_staged(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    secret: &str,
) -> Result<Option<StagedLogin>, sqlx::Error> {
    let Some(row) = sqlx::query!(
        "select id, secret_hash, provider, identity, existing_user_id, org_ids, \
                return_to, user_agent, created_ip, created_port, expires_at \
         from tml_switchboard.staged_logins where id = $1 for update;",
        id,
    )
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(None);
    };

    if !staged_secret::verify(secret, &row.secret_hash) {
        return Ok(None);
    }

    // Secret holder confirmed: consume the row whether it is still live or
    // already expired (deleting an expired row is just the sweep, early).
    sqlx::query!(
        "delete from tml_switchboard.staged_logins where id = $1;",
        id,
    )
    .execute(&mut **tx)
    .await?;

    Ok((row.expires_at > Utc::now()).then_some(StagedLogin {
        id: row.id,
        provider: row.provider,
        identity: row.identity,
        existing_user_id: row.existing_user_id,
        org_ids: row.org_ids,
        return_to: row.return_to,
        user_agent: row.user_agent,
        created_ip: row.created_ip,
        created_port: row.created_port,
        expires_at: row.expires_at,
    }))
}

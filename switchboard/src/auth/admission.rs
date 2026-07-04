//! The login admission gate.
//!
//! Decides whether a brand-new external identity may create a local account.
//! Consulted by the login callback ([`crate::routes::auth`]) ONLY on the
//! truly-new-subject path (see [`crate::sql::user::resolve_user`]); existing
//! users -- including a new identity linked to an existing account by verified
//! email -- are never gated.
//!
//! The decision is hidden behind [`AdmissionPolicy`] so a future config- or
//! other-backend-driven policy is a localized swap. The shipped policy
//! ([`DbAdmissionPolicy`]) is a read-only consult of the hand-managed
//! `login_allowlist` table.

use async_trait::async_trait;
use sqlx::PgPool;

use crate::auth::oauth::ExternalIdentity;

/// The gate's verdict for a new-user registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// The identity may create an account.
    Admit,
    /// The identity is refused; carries the reason for the audit trail.
    Deny(DenyReason),
}

/// Why a registration was denied. Each variant has a stable string form
/// ([`DenyReason::as_str`]) recorded on the `registration_denied` audit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// The identity is neither individually allow-listed nor a member of any
    /// allow-listed org. Produced by [`AdmissionPolicy::admit`].
    NotAllowlisted,
    /// The provider org lookup that org-based admission depends on failed. NOT
    /// produced by [`AdmissionPolicy::admit`] -- the login callback raises it
    /// when `fetch_org_ids` errors on the new-user path, failing closed. The
    /// registration is retryable (a transient failure never persists anything).
    OrgLookupFailed,
}

impl DenyReason {
    /// The stable identifier recorded in the audit payload's `reason` field.
    pub fn as_str(&self) -> &'static str {
        match self {
            DenyReason::NotAllowlisted => "not_allowlisted",
            DenyReason::OrgLookupFailed => "org_lookup_failed",
        }
    }
}

/// A pluggable admission policy for new-user registrations.
#[async_trait]
pub trait AdmissionPolicy {
    /// Decide whether `identity` (a brand-new subject) may register. `org_ids`
    /// are the provider org ids the identity currently belongs to, already
    /// resolved by the caller.
    async fn admit(
        &self,
        provider: &str,
        identity: &ExternalIdentity,
        org_ids: &[String],
    ) -> Result<Admission, sqlx::Error>;
}

/// The shipped admission policy: a single read-only `EXISTS` consult of the
/// hand-managed `login_allowlist` table. An identity is admitted if it is
/// individually allow-listed (`kind = 'user'`) or a member of an allow-listed
/// org (`kind = 'org'`); otherwise it is denied [`DenyReason::NotAllowlisted`].
pub struct DbAdmissionPolicy {
    pool: PgPool,
}

impl DbAdmissionPolicy {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl AdmissionPolicy for DbAdmissionPolicy {
    async fn admit(
        &self,
        provider: &str,
        identity: &ExternalIdentity,
        org_ids: &[String],
    ) -> Result<Admission, sqlx::Error> {
        let admitted = sqlx::query_scalar!(
            r#"select exists (
                select 1 from tml_switchboard.login_allowlist
                where provider = $1
                  and ( (kind = 'user' and external_id = $2)
                     or (kind = 'org' and external_id = any($3::text[])) )
            ) as "admitted!";"#,
            provider,
            identity.provider_user_id,
            org_ids,
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(if admitted {
            Admission::Admit
        } else {
            Admission::Deny(DenyReason::NotAllowlisted)
        })
    }
}

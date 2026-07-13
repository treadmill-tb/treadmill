//! User provisioning from an external OAuth identity, plus auto-group
//! reconciliation.
//!
//! Every state change here rides through the audit chokepoint
//! ([`audit::transition`]) so the login flow leaves a gapless, attributable
//! trail. Pure lookups (resolve-by-identity, link-by-email, the reconcile
//! diff) stay plain reads.

use std::borrow::Cow;

use crate::audit::model::Subject as AuditSubject;
use crate::audit::{self, Transition, WriteToken, events};
use crate::auth::oauth::ExternalIdentity;
use sqlx::{PgConnection, Postgres, Transaction};
use uuid::Uuid;

/// How [`resolve_user`] matched an external identity to an existing local user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveKind {
    /// The stable provider identity is already linked to a local user.
    IdentityMatch,
    /// No identity match, but exactly one existing user owns a verified email
    /// this identity also reports; the identity links to that account.
    EmailLink,
}

/// Resolve the local user for an external identity WITHOUT mutating anything.
///
/// This is the read-only front half of login provisioning, split from the
/// create/refresh half so the login callback can consult the admission gate on
/// the truly-new-subject path (a `None` return) before any record exists. Two
/// ways to match:
///   1. the stable provider identity is already linked ([`ResolveKind::IdentityMatch`]), or
///   2. exactly one existing user owns a verified email this identity also
///      reports ([`ResolveKind::EmailLink`]) -- the identity will be linked to
///      that account on the refresh path.
///
/// Zero matches -> `None` (brand new). More than one verified-email owner ->
/// `None` too: ambiguous, and we never auto-merge two accounts that merely share
/// an address, so the identity is treated as a new registration (and gated).
pub async fn resolve_user(
    conn: &mut PgConnection,
    provider: &str,
    identity: &ExternalIdentity,
) -> Result<Option<(Uuid, ResolveKind)>, sqlx::Error> {
    // 1. Resolve by the stable provider identity.
    let by_identity = sqlx::query_scalar!(
        "select user_id from tml_switchboard.user_identities \
         where provider = $1 and provider_user_id = $2;",
        provider,
        identity.provider_user_id,
    )
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(uid) = by_identity {
        return Ok(Some((uid, ResolveKind::IdentityMatch)));
    }

    // 2. Otherwise, try to link by a shared verified email.
    let verified_emails: Vec<&str> = identity
        .emails
        .iter()
        .filter_map(|e| {
            if e.verified {
                Some(e.address.as_ref())
            } else {
                None
            }
        })
        .collect();
    let linked: Vec<Uuid> = if verified_emails.is_empty() {
        Vec::new()
    } else {
        sqlx::query_scalar!(
            "select distinct user_id from tml_switchboard.user_emails \
             where verified = true and email = any($1::text[]);",
            verified_emails as _,
        )
        .fetch_all(&mut *conn)
        .await?
    };

    match linked.len() {
        1 => Ok(Some((linked[0], ResolveKind::EmailLink))),
        0 => Ok(None),
        n => {
            // Ambiguous: do NOT auto-merge two accounts that merely share an
            // address. Treat as a brand-new registration (subject to the gate).
            tracing::warn!(
                "verified emails for {provider} identity {} match {} existing users; \
                 treating as a new registration instead of auto-merging",
                identity.provider_user_id,
                n,
            );
            Ok(None)
        }
    }
}

/// Create a brand-new local account from an external identity, then refresh its
/// profile, record its verified emails, and reconcile its auto-group
/// memberships. Returns the new user's subject id.
///
/// `tos_version` is the Terms of Service version the user accepted to reach this
/// point; it is stamped onto the new row (`tos_accepted_version`/
/// `tos_accepted_at`) so the account starts out consented. Only reached after
/// the admission gate admits the identity AND the ToS is accepted (the
/// interstitial's accept handler); [`resolve_user`] must have returned `None`
/// first. Runs on the caller's transaction so the whole provisioning is atomic
/// with the session-token issuance and its audit events.
pub async fn create_and_reconcile(
    tx: &mut Transaction<'_, Postgres>,
    provider: &str,
    identity: &ExternalIdentity,
    org_ids: &[String],
    tos_version: i32,
) -> Result<Uuid, sqlx::Error> {
    let user_id = audit::transition(
        tx,
        CreateUser {
            provider,
            identity,
            tos_version,
        },
    )
    .await?;
    refresh_and_reconcile(tx, provider, identity, org_ids, user_id).await?;
    Ok(user_id)
}

/// Refresh an already-resolved existing user on login: link a newly-seen
/// identity (only for an [`ResolveKind::EmailLink`] resolution), refresh mutable
/// profile fields, record any new verified emails, and reconcile auto-group
/// memberships. This is the ungated path -- existing users are never subject to
/// the admission gate.
///
/// Runs on the caller's transaction so the whole refresh is atomic with the
/// session-token issuance and its audit events.
pub async fn provision_existing_user(
    tx: &mut Transaction<'_, Postgres>,
    provider: &str,
    identity: &ExternalIdentity,
    org_ids: &[String],
    user_id: Uuid,
    kind: ResolveKind,
) -> Result<(), sqlx::Error> {
    if kind == ResolveKind::EmailLink {
        audit::transition(
            tx,
            LinkIdentity {
                user_id,
                provider,
                identity,
            },
        )
        .await?;
    }
    refresh_and_reconcile(tx, provider, identity, org_ids, user_id).await
}

/// The shared create/refresh tail: refresh the mutable profile, record new
/// verified emails, and reconcile auto-group memberships for `user_id`. The
/// identity row for `(provider, identity.provider_user_id)` must already exist
/// (created by [`CreateUser`] or [`LinkIdentity`]).
async fn refresh_and_reconcile(
    tx: &mut Transaction<'_, Postgres>,
    provider: &str,
    identity: &ExternalIdentity,
    org_ids: &[String],
    user_id: Uuid,
) -> Result<(), sqlx::Error> {
    // 3. Refresh the provider-sourced mutable fields (avatar, recorded provider
    // login), but only when something actually changed, so a no-op re-login
    // does not spam the audit log. The display name is user-chosen and never
    // overwritten on login.
    let current = sqlx::query!(
        "select u.avatar_url, i.provider_login \
         from tml_switchboard.users u \
         join tml_switchboard.user_identities i \
           on i.user_id = u.subject_id and i.provider = $2 and i.provider_user_id = $3 \
         where u.subject_id = $1;",
        user_id,
        provider,
        identity.provider_user_id,
    )
    .fetch_one(&mut **tx)
    .await?;

    if current.avatar_url != identity.avatar_url
        || current.provider_login.as_deref() != Some(identity.login.as_str())
    {
        audit::transition(
            tx,
            ChangeProfile {
                user_id,
                provider,
                identity,
                old_avatar_url: current.avatar_url,
                old_provider_login: current.provider_login,
            },
        )
        .await?;
    }

    // 4. Reconcile recorded emails against the provider's current report,
    // scoped to THIS provider's rows: record new addresses, align the
    // `verified` flag in both directions, and remove addresses the provider no
    // longer reports. Rows recorded under other providers are untouched, and
    // an address already on file elsewhere (another user's, or this user's
    // under another provider) is left alone and unlogged, as before.
    let current: Vec<(String, bool)> = sqlx::query!(
        "select email, verified from tml_switchboard.user_emails \
         where user_id = $1 and provider = $2;",
        user_id,
        provider,
    )
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|r| (r.email, r.verified))
    .collect();

    let present_elsewhere: Vec<Cow<str>> = if identity.emails.is_empty() {
        Vec::new()
    } else {
        let all_emails: Vec<&str> = identity.emails.iter().map(|e| e.address.as_ref()).collect();
        sqlx::query_scalar!(
            "select email from tml_switchboard.user_emails \
             where email = any($1::text[]) and not (user_id = $2 and provider = $3);",
            &all_emails as _,
            user_id,
            provider,
        )
        .fetch_all(&mut **tx)
        .await?
        .into_iter()
        .map(|email| email.into())
        .collect()
    };

    for email in &identity.emails {
        match current.iter().find(|(addr, _)| *addr == email.address) {
            Some((_, verified)) if *verified != email.verified => {
                audit::transition(
                    tx,
                    ChangeEmailVerified {
                        user_id,
                        provider,
                        email: &email.address,
                        verified: email.verified,
                    },
                )
                .await?;
            }
            Some(_) => {}
            None if !present_elsewhere.contains(&email.address) => {
                audit::transition(
                    tx,
                    AddEmail {
                        user_id,
                        provider,
                        email: &email.address,
                        verified: email.verified,
                    },
                )
                .await?;
            }
            None => {}
        }
    }

    for (addr, _) in &current {
        if !identity.emails.iter().any(|e| e.address == *addr) {
            audit::transition(
                tx,
                RemoveEmail {
                    user_id,
                    provider,
                    email: addr,
                },
            )
            .await?;
        }
    }

    // 5. Reconcile auto-group memberships from current org membership.
    reconcile_auto_groups(tx, user_id, provider, org_ids).await?;

    Ok(())
}

/// Create a fresh subject + user + identity. The display name is seeded from
/// the provider's display name, falling back to the login handle. Emits
/// [`events::UserProvisioned`].
struct CreateUser<'a> {
    provider: &'a str,
    identity: &'a ExternalIdentity,
    /// The ToS version the user accepted; stamped onto the new row so the
    /// account is created already-consented.
    tos_version: i32,
}

impl Transition for CreateUser<'_> {
    type Output = Uuid;
    type Event = events::UserProvisioned;

    async fn apply(
        self,
        conn: &mut PgConnection,
        _w: &WriteToken,
    ) -> Result<(Self::Output, Self::Event), sqlx::Error> {
        // Time-ordered (v7) for primary-key insert locality.
        let user_id = Uuid::now_v7();
        sqlx::query!(
            "insert into tml_switchboard.subjects (subject_id, kind) values ($1, 'user');",
            user_id,
        )
        .execute(&mut *conn)
        .await?;

        let name = self
            .identity
            .full_name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| self.identity.login.clone());
        sqlx::query!(
            "insert into tml_switchboard.users \
             (subject_id, name, avatar_url, tos_accepted_version, tos_accepted_at) \
             values ($1, $2, $3, $4, now());",
            user_id,
            name,
            self.identity.avatar_url,
            self.tos_version,
        )
        .execute(&mut *conn)
        .await?;

        sqlx::query!(
            "insert into tml_switchboard.user_identities \
             (provider, provider_user_id, user_id, provider_login) \
             values ($1, $2, $3, $4);",
            self.provider,
            self.identity.provider_user_id,
            user_id,
            self.identity.login,
        )
        .execute(&mut *conn)
        .await?;

        let event = events::UserProvisioned {
            actor: AuditSubject(user_id),
            user: AuditSubject(user_id),
            provider: self.provider.to_string(),
            provider_user_id: self.identity.provider_user_id.clone(),
            login: self.identity.login.clone(),
            name,
        };
        Ok((user_id, event))
    }
}

/// Link a new external identity to an existing user matched by verified email.
/// Emits [`events::OAuthIdentityLinked`].
struct LinkIdentity<'a> {
    user_id: Uuid,
    provider: &'a str,
    identity: &'a ExternalIdentity,
}

impl Transition for LinkIdentity<'_> {
    type Output = ();
    type Event = events::OAuthIdentityLinked;

    async fn apply(
        self,
        conn: &mut PgConnection,
        _w: &WriteToken,
    ) -> Result<(Self::Output, Self::Event), sqlx::Error> {
        sqlx::query!(
            "insert into tml_switchboard.user_identities \
             (provider, provider_user_id, user_id, provider_login) \
             values ($1, $2, $3, $4);",
            self.provider,
            self.identity.provider_user_id,
            self.user_id,
            self.identity.login,
        )
        .execute(&mut *conn)
        .await?;

        let event = events::OAuthIdentityLinked {
            actor: AuditSubject(self.user_id),
            user: AuditSubject(self.user_id),
            provider: self.provider.to_string(),
            provider_user_id: self.identity.provider_user_id.clone(),
            login: self.identity.login.clone(),
        };
        Ok(((), event))
    }
}

/// Refresh the provider-sourced mutable fields (avatar, recorded provider
/// login handle). The user-chosen display name is never touched here. Emits
/// [`events::UserProfileChanged`] carrying the prior and new values. Only
/// constructed when a field actually differs.
struct ChangeProfile<'a> {
    user_id: Uuid,
    provider: &'a str,
    identity: &'a ExternalIdentity,
    old_avatar_url: Option<String>,
    old_provider_login: Option<String>,
}

impl Transition for ChangeProfile<'_> {
    type Output = ();
    type Event = events::UserProfileChanged;

    async fn apply(
        self,
        conn: &mut PgConnection,
        _w: &WriteToken,
    ) -> Result<(Self::Output, Self::Event), sqlx::Error> {
        sqlx::query!(
            "update tml_switchboard.users set avatar_url = $2 \
             where subject_id = $1;",
            self.user_id,
            self.identity.avatar_url,
        )
        .execute(&mut *conn)
        .await?;
        sqlx::query!(
            "update tml_switchboard.user_identities set provider_login = $3 \
             where provider = $1 and provider_user_id = $2;",
            self.provider,
            self.identity.provider_user_id,
            self.identity.login,
        )
        .execute(&mut *conn)
        .await?;

        let event = events::UserProfileChanged {
            actor: AuditSubject(self.user_id),
            user: AuditSubject(self.user_id),
            old_avatar_url: self.old_avatar_url,
            new_avatar_url: self.identity.avatar_url.clone(),
            old_provider_login: self.old_provider_login,
            new_provider_login: Some(self.identity.login.clone()),
        };
        Ok(((), event))
    }
}

/// Record a verified email for the user, claiming it only if unowned. Emits
/// [`events::UserEmailAdded`]. Constructed only for addresses not already on
/// file, but keeps `on conflict do nothing` to stay safe under a concurrent
/// login racing to insert the same address.
struct AddEmail<'a> {
    user_id: Uuid,
    provider: &'a str,
    email: &'a str,
    verified: bool,
}

impl Transition for AddEmail<'_> {
    type Output = ();
    type Event = events::UserEmailAdded;

    async fn apply(
        self,
        conn: &mut PgConnection,
        _w: &WriteToken,
    ) -> Result<(Self::Output, Self::Event), sqlx::Error> {
        sqlx::query!(
            "insert into tml_switchboard.user_emails (email, user_id, provider, verified) \
             values ($1, $2, $3, $4) on conflict (email) do nothing;",
            self.email,
            self.user_id,
            self.provider,
            self.verified,
        )
        .execute(&mut *conn)
        .await?;

        let event = events::UserEmailAdded {
            actor: AuditSubject(self.user_id),
            user: AuditSubject(self.user_id),
            provider: self.provider.to_string(),
            email: self.email.to_string(),
            verified: self.verified,
        };
        Ok(((), event))
    }
}

/// Align a recorded email's `verified` flag with the provider's current
/// report. Constructed only when the flag actually differs. Emits
/// [`events::UserEmailVerificationChanged`].
struct ChangeEmailVerified<'a> {
    user_id: Uuid,
    provider: &'a str,
    email: &'a str,
    verified: bool,
}

impl Transition for ChangeEmailVerified<'_> {
    type Output = ();
    type Event = events::UserEmailVerificationChanged;

    async fn apply(
        self,
        conn: &mut PgConnection,
        _w: &WriteToken,
    ) -> Result<(Self::Output, Self::Event), sqlx::Error> {
        sqlx::query!(
            "update tml_switchboard.user_emails set verified = $4 \
             where email = $1 and user_id = $2 and provider = $3;",
            self.email,
            self.user_id,
            self.provider,
            self.verified,
        )
        .execute(&mut *conn)
        .await?;

        let event = events::UserEmailVerificationChanged {
            actor: AuditSubject(self.user_id),
            user: AuditSubject(self.user_id),
            provider: self.provider.to_string(),
            email: self.email.to_string(),
            verified: self.verified,
        };
        Ok(((), event))
    }
}

/// Remove a recorded email the provider no longer reports for this identity.
/// Emits [`events::UserEmailRemoved`].
struct RemoveEmail<'a> {
    user_id: Uuid,
    provider: &'a str,
    email: &'a str,
}

impl Transition for RemoveEmail<'_> {
    type Output = ();
    type Event = events::UserEmailRemoved;

    async fn apply(
        self,
        conn: &mut PgConnection,
        _w: &WriteToken,
    ) -> Result<(Self::Output, Self::Event), sqlx::Error> {
        sqlx::query!(
            "delete from tml_switchboard.user_emails \
             where email = $1 and user_id = $2 and provider = $3;",
            self.email,
            self.user_id,
            self.provider,
        )
        .execute(&mut *conn)
        .await?;

        let event = events::UserEmailRemoved {
            actor: AuditSubject(self.user_id),
            user: AuditSubject(self.user_id),
            provider: self.provider.to_string(),
            email: self.email.to_string(),
        };
        Ok(((), event))
    }
}

/// Add a single `github_org`-sourced group membership. Emits
/// [`events::GroupMembershipChanged`] with `added = true`.
struct AddGroupMembership {
    user_id: Uuid,
    group_id: Uuid,
    source_ref: String,
}

impl Transition for AddGroupMembership {
    type Output = ();
    type Event = events::GroupMembershipChanged;

    async fn apply(
        self,
        conn: &mut PgConnection,
        _w: &WriteToken,
    ) -> Result<(Self::Output, Self::Event), sqlx::Error> {
        sqlx::query!(
            "insert into tml_switchboard.group_members (group_id, member_id, source, source_ref) \
             values ($1, $2, 'github_org', $3) \
             on conflict (group_id, member_id, source, source_ref) do nothing;",
            self.group_id,
            self.user_id,
            self.source_ref,
        )
        .execute(&mut *conn)
        .await?;

        let event = events::GroupMembershipChanged {
            actor: AuditSubject(self.user_id),
            user: AuditSubject(self.user_id),
            group: AuditSubject(self.group_id),
            source_ref: self.source_ref,
            added: true,
        };
        Ok(((), event))
    }
}

/// Remove a single `github_org`-sourced group membership. Emits
/// [`events::GroupMembershipChanged`] with `added = false`.
struct RemoveGroupMembership {
    user_id: Uuid,
    group_id: Uuid,
    source_ref: String,
}

impl Transition for RemoveGroupMembership {
    type Output = ();
    type Event = events::GroupMembershipChanged;

    async fn apply(
        self,
        conn: &mut PgConnection,
        _w: &WriteToken,
    ) -> Result<(Self::Output, Self::Event), sqlx::Error> {
        sqlx::query!(
            "delete from tml_switchboard.group_members \
             where member_id = $1 and source = 'github_org' \
               and group_id = $2 and source_ref = $3;",
            self.user_id,
            self.group_id,
            self.source_ref,
        )
        .execute(&mut *conn)
        .await?;

        let event = events::GroupMembershipChanged {
            actor: AuditSubject(self.user_id),
            user: AuditSubject(self.user_id),
            group: AuditSubject(self.group_id),
            source_ref: self.source_ref,
            added: false,
        };
        Ok(((), event))
    }
}

/// Update a user's display name and/or avatar via the management API. Writes
/// the final values for both columns (the route fills the unchanged column with
/// its current value). Emits [`events::UserProfileUpdated`], distinct from the
/// login-time [`events::UserProfileChanged`].
pub struct UpdateUserProfile {
    pub user_id: Uuid,
    pub old_name: String,
    pub new_name: String,
    pub old_avatar_url: Option<String>,
    pub new_avatar_url: Option<String>,
}

impl Transition for UpdateUserProfile {
    type Output = ();
    type Event = events::UserProfileUpdated;

    async fn apply(
        self,
        conn: &mut PgConnection,
        _w: &WriteToken,
    ) -> Result<(Self::Output, Self::Event), sqlx::Error> {
        sqlx::query!(
            "update tml_switchboard.users set name = $2, avatar_url = $3 \
             where subject_id = $1;",
            self.user_id,
            self.new_name,
            self.new_avatar_url,
        )
        .execute(&mut *conn)
        .await?;

        let event = events::UserProfileUpdated {
            actor: AuditSubject(self.user_id),
            user: AuditSubject(self.user_id),
            old_name: self.old_name,
            new_name: self.new_name,
            old_avatar_url: self.old_avatar_url,
            new_avatar_url: self.new_avatar_url,
        };
        Ok(((), event))
    }
}

/// Reconcile this user's `github_org`-sourced group memberships against the set
/// of org ids they currently belong to. Computes the add/remove deltas as plain
/// reads, then routes each individual change through the audit chokepoint so
/// every membership mutation is logged. Only ever touches `source = 'github_org'`
/// rows, so manual memberships are preserved.
async fn reconcile_auto_groups(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    provider: &str,
    org_ids: &[String],
) -> Result<(), sqlx::Error> {
    let desired = sqlx::query!(
        "select gas.group_id, gas.external_id as source_ref \
         from tml_switchboard.group_auto_sources gas \
         where gas.provider = $1 and gas.external_id = any($2::text[]);",
        provider,
        org_ids,
    )
    .fetch_all(&mut **tx)
    .await?;

    let current = sqlx::query!(
        "select group_id, source_ref \
         from tml_switchboard.group_members \
         where member_id = $1 and source = 'github_org';",
        user_id,
    )
    .fetch_all(&mut **tx)
    .await?;

    for d in &desired {
        let already = current
            .iter()
            .any(|c| c.group_id == d.group_id && c.source_ref == d.source_ref);
        if !already {
            audit::transition(
                tx,
                AddGroupMembership {
                    user_id,
                    group_id: d.group_id,
                    source_ref: d.source_ref.clone(),
                },
            )
            .await?;
        }
    }

    for c in &current {
        let still_desired = desired
            .iter()
            .any(|d| d.group_id == c.group_id && d.source_ref == c.source_ref);
        if !still_desired {
            audit::transition(
                tx,
                RemoveGroupMembership {
                    user_id,
                    group_id: c.group_id,
                    source_ref: c.source_ref.clone(),
                },
            )
            .await?;
        }
    }

    Ok(())
}

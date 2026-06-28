//! User provisioning from an external OAuth identity, plus auto-group
//! reconciliation.
//!
//! Every state change here rides through the audit chokepoint
//! ([`audit::transition`]) so the login flow leaves a gapless, attributable
//! trail. Pure lookups (resolve-by-identity, link-by-email, username
//! de-duplication, the reconcile diff) stay plain reads.

use std::borrow::Cow;

use crate::audit::model::Subject as AuditSubject;
use crate::audit::{self, Transition, WriteToken, events};
use crate::auth::engine::ADMINS_GROUP_ID;
use crate::auth::oauth::ExternalIdentity;
use sqlx::{PgConnection, Postgres, Transaction};
use uuid::Uuid;

/// Resolve (or create) the local user for an external identity, refresh its
/// mutable profile, record its verified emails, and reconcile its auto-group
/// memberships. Returns the user's subject id and whether the account was
/// freshly created by this call.
///
/// Runs entirely on the caller's transaction so the whole login provisioning is
/// atomic with the session-token issuance and its audit events.
pub async fn provision_user(
    tx: &mut Transaction<'_, Postgres>,
    provider: &str,
    identity: &ExternalIdentity,
    org_ids: &[String],
) -> Result<(Uuid, bool), sqlx::Error> {
    // 1. Resolve by the stable provider identity (read only).
    let by_identity = sqlx::query_scalar!(
        "select user_id from tml_switchboard.user_identities \
         where provider = $1 and provider_user_id = $2;",
        provider,
        identity.provider_user_id,
    )
    .fetch_optional(&mut **tx)
    .await?;

    let (user_id, new_user) = match by_identity {
        Some(uid) => (uid, false),
        None => {
            // 2. Otherwise, try to link by a shared verified email (read only).
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
                .fetch_all(&mut **tx)
                .await?
            };

            if linked.len() == 1 {
                let uid = linked[0];
                audit::transition(
                    tx,
                    LinkIdentity {
                        user_id: uid,
                        provider,
                        identity,
                    },
                )
                .await?;
                (uid, false)
            } else {
                // Zero matches -> brand new user. More than one -> ambiguous; do
                // NOT auto-merge two accounts that merely share an address.
                if linked.len() > 1 {
                    tracing::warn!(
                        "verified emails for {provider} identity {} match {} existing users; \
                         creating a fresh user instead of auto-merging",
                        identity.provider_user_id,
                        linked.len(),
                    );
                }
                let uid = audit::transition(tx, CreateUser { provider, identity }).await?;
                (uid, true)
            }
        }
    };

    // 3. Refresh mutable profile fields, but only when something actually
    // changed, so a no-op re-login does not spam the audit log.
    let current = sqlx::query!(
        "select u.full_name, u.avatar_url, i.provider_login \
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

    if current.full_name != identity.full_name
        || current.avatar_url != identity.avatar_url
        || current.provider_login.as_deref() != Some(identity.login.as_str())
    {
        audit::transition(
            tx,
            ChangeProfile {
                user_id,
                provider,
                identity,
                old_full_name: current.full_name,
                old_avatar_url: current.avatar_url,
                old_provider_login: current.provider_login,
            },
        )
        .await?;
    }

    // 4. Record emails from the identify, emitting an event only for ones
    // genuinely new to the system (an address already on file, even another
    // user's, is left untouched and unlogged).
    //
    // TODO: this should also mark previously unverified emails as verified,
    // remove emails when they vanish from the OAuth provider, and revoke the
    // "verified" attribute when an email is no longer reported as verified by
    // the upstream provider.
    let present: Vec<Cow<str>> = if identity.emails.is_empty() {
        Vec::new()
    } else {
        let all_emails: Vec<&str> = identity.emails.iter().map(|e| e.address.as_ref()).collect();
        sqlx::query_scalar!(
            "select email from tml_switchboard.user_emails where email = any($1::text[]);",
            &all_emails as _,
        )
        .fetch_all(&mut **tx)
        .await?
        .into_iter()
        .map(|email| email.into())
        .collect()
    };
    for email in &identity.emails {
        if !present.contains(&email.address) {
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
    }

    // 5. Reconcile auto-group memberships from current org membership.
    reconcile_auto_groups(tx, user_id, provider, org_ids).await?;

    Ok((user_id, new_user))
}

/// Create a fresh subject + user + identity. The username is suggested from the
/// provider login and de-duplicated on collision. Emits [`events::UserProvisioned`].
struct CreateUser<'a> {
    provider: &'a str,
    identity: &'a ExternalIdentity,
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

        let username = unique_username(&mut *conn, &self.identity.login).await?;
        sqlx::query!(
            "insert into tml_switchboard.users (subject_id, username, full_name, avatar_url) \
             values ($1, $2, $3, $4);",
            user_id,
            username,
            self.identity.full_name,
            self.identity.avatar_url,
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
            username,
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

/// Refresh the mutable profile fields (display name, avatar) and the recorded
/// provider login handle. Emits [`events::UserProfileChanged`] carrying the
/// prior and new values. Only constructed when a field actually differs.
struct ChangeProfile<'a> {
    user_id: Uuid,
    provider: &'a str,
    identity: &'a ExternalIdentity,
    old_full_name: Option<String>,
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
            "update tml_switchboard.users set full_name = $2, avatar_url = $3 \
             where subject_id = $1;",
            self.user_id,
            self.identity.full_name,
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
            old_full_name: self.old_full_name,
            new_full_name: self.identity.full_name.clone(),
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

/// Change a user's username via the management API. The new value is assumed
/// pre-validated (shape + reserved-name checks happen in the route); uniqueness
/// is enforced by the DB and surfaces here as a unique-violation error the route
/// turns into a clean 409. Emits [`events::UserRenamed`].
pub struct RenameUser {
    pub user_id: Uuid,
    pub old_username: String,
    pub new_username: String,
}

impl Transition for RenameUser {
    type Output = ();
    type Event = events::UserRenamed;

    async fn apply(
        self,
        conn: &mut PgConnection,
        _w: &WriteToken,
    ) -> Result<(Self::Output, Self::Event), sqlx::Error> {
        sqlx::query!(
            "update tml_switchboard.users set username = $2 where subject_id = $1;",
            self.user_id,
            self.new_username,
        )
        .execute(&mut *conn)
        .await?;

        let event = events::UserRenamed {
            actor: AuditSubject(self.user_id),
            user: AuditSubject(self.user_id),
            old_username: self.old_username,
            new_username: self.new_username,
        };
        Ok(((), event))
    }
}

/// Update a user's display name and/or avatar via the management API. Writes the
/// final values for both columns (the route fills the unchanged column with its
/// current value). Emits [`events::UserProfileUpdated`], distinct from the
/// login-time [`events::UserProfileChanged`].
pub struct UpdateUserProfile {
    pub user_id: Uuid,
    pub old_full_name: Option<String>,
    pub new_full_name: Option<String>,
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
            "update tml_switchboard.users set full_name = $2, avatar_url = $3 \
             where subject_id = $1;",
            self.user_id,
            self.new_full_name,
            self.new_avatar_url,
        )
        .execute(&mut *conn)
        .await?;

        let event = events::UserProfileUpdated {
            actor: AuditSubject(self.user_id),
            user: AuditSubject(self.user_id),
            old_full_name: self.old_full_name,
            new_full_name: self.new_full_name,
            old_avatar_url: self.old_avatar_url,
            new_avatar_url: self.new_avatar_url,
        };
        Ok(((), event))
    }
}

/// Find an unused username, trying `base`, then `base-2`, `base-3`, ...
async fn unique_username(conn: &mut PgConnection, base: &str) -> Result<String, sqlx::Error> {
    let mut candidate = base.to_string();
    let mut n = 1u32;
    loop {
        let taken = sqlx::query_scalar!(
            "select exists(select 1 from tml_switchboard.users where username = $1);",
            candidate,
        )
        .fetch_one(&mut *conn)
        .await?
        .unwrap_or(false);
        if !taken {
            return Ok(candidate);
        }
        n += 1;
        candidate = format!("{base}-{n}");
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

/// Idempotently make `user_id` a member of the global `admins` group via a
/// `manual`-source membership. Used ONLY by the development-only mock provider's
/// admin identity (see [`crate::auth::oauth::mock`]); checks current membership
/// as a plain read first so a no-op re-login does not spam the audit log.
pub async fn ensure_global_admin(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
) -> Result<(), sqlx::Error> {
    let already = sqlx::query_scalar!(
        "select exists(\
           select 1 from tml_switchboard.group_members \
           where group_id = $1 and member_id = $2\
         ) as \"exists!\";",
        ADMINS_GROUP_ID,
        user_id,
    )
    .fetch_one(&mut **tx)
    .await?;

    if !already {
        audit::transition(tx, GrantGlobalAdmin { user_id }).await?;
    }
    Ok(())
}

/// Add `user_id` to the global `admins` group as a `manual` membership. Emits
/// [`events::GroupMembershipChanged`] with `added = true`. `on conflict do
/// nothing` keeps it safe under a concurrent login racing to insert the same row.
struct GrantGlobalAdmin {
    user_id: Uuid,
}

impl Transition for GrantGlobalAdmin {
    type Output = ();
    type Event = events::GroupMembershipChanged;

    async fn apply(
        self,
        conn: &mut PgConnection,
        _w: &WriteToken,
    ) -> Result<(Self::Output, Self::Event), sqlx::Error> {
        sqlx::query!(
            "insert into tml_switchboard.group_members (group_id, member_id, source, source_ref) \
             values ($1, $2, 'manual', '') on conflict do nothing;",
            ADMINS_GROUP_ID,
            self.user_id,
        )
        .execute(&mut *conn)
        .await?;

        let event = events::GroupMembershipChanged {
            actor: AuditSubject(self.user_id),
            user: AuditSubject(self.user_id),
            group: AuditSubject(ADMINS_GROUP_ID),
            source_ref: String::new(),
            added: true,
        };
        Ok(((), event))
    }
}

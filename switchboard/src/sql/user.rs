//! User provisioning from an external OAuth identity, plus auto-group
//! reconciliation. See `OAUTH_LOGIN_PLAN.md` §6 and §9.

use crate::auth::oauth::ExternalIdentity;
use sqlx::PgConnection;
use uuid::Uuid;

/// Resolve (or create) the local user for an external identity, refresh its
/// mutable profile, record its verified emails, and reconcile its auto-group
/// memberships. Returns the user's subject id.
///
/// Runs entirely on the caller's transaction so the whole login provisioning is
/// atomic with the session-token issuance.
pub async fn provision_user(
    conn: &mut PgConnection,
    provider: &str,
    identity: &ExternalIdentity,
    org_ids: &[String],
) -> Result<Uuid, sqlx::Error> {
    // 1. Resolve by the stable provider identity.
    let by_identity = sqlx::query_scalar!(
        "select user_id from tml_switchboard.user_identities \
         where provider = $1 and provider_user_id = $2;",
        provider,
        identity.provider_user_id,
    )
    .fetch_optional(&mut *conn)
    .await?;

    let user_id = match by_identity {
        Some(uid) => uid,
        None => {
            // 2. Otherwise, try to link by a shared verified email.
            let linked: Vec<Uuid> = if identity.verified_emails.is_empty() {
                Vec::new()
            } else {
                sqlx::query_scalar!(
                    "select distinct user_id from tml_switchboard.user_emails \
                     where email = any($1::text[]);",
                    &identity.verified_emails,
                )
                .fetch_all(&mut *conn)
                .await?
            };

            if linked.len() == 1 {
                let uid = linked[0];
                sqlx::query!(
                    "insert into tml_switchboard.user_identities \
                     (provider, provider_user_id, user_id, provider_login) \
                     values ($1, $2, $3, $4);",
                    provider,
                    identity.provider_user_id,
                    uid,
                    identity.login,
                )
                .execute(&mut *conn)
                .await?;
                uid
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
                create_user(&mut *conn, provider, identity).await?
            }
        }
    };

    // 4. Refresh mutable profile fields and the recorded provider login handle.
    sqlx::query!(
        "update tml_switchboard.users set full_name = $2, avatar_url = $3 \
         where subject_id = $1;",
        user_id,
        identity.full_name,
        identity.avatar_url,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "update tml_switchboard.user_identities set provider_login = $3 \
         where provider = $1 and provider_user_id = $2;",
        provider,
        identity.provider_user_id,
        identity.login,
    )
    .execute(&mut *conn)
    .await?;

    // 5. Record verified emails, never stealing one already owned by another.
    for email in &identity.verified_emails {
        sqlx::query!(
            "insert into tml_switchboard.user_emails (email, user_id, provider, verified) \
             values ($1, $2, $3, true) on conflict (email) do nothing;",
            email,
            user_id,
            provider,
        )
        .execute(&mut *conn)
        .await?;
    }

    // 6. Reconcile auto-group memberships from current org membership.
    reconcile_auto_groups(&mut *conn, user_id, provider, org_ids).await?;

    Ok(user_id)
}

/// Create a fresh subject + user + identity. `username` is suggested from the
/// provider login and de-duplicated on collision.
async fn create_user(
    conn: &mut PgConnection,
    provider: &str,
    identity: &ExternalIdentity,
) -> Result<Uuid, sqlx::Error> {
    let user_id = Uuid::new_v4();
    sqlx::query!(
        "insert into tml_switchboard.subjects (subject_id, kind) values ($1, 'user');",
        user_id,
    )
    .execute(&mut *conn)
    .await?;

    let username = unique_username(&mut *conn, &identity.login).await?;
    sqlx::query!(
        "insert into tml_switchboard.users (subject_id, username, full_name, avatar_url) \
         values ($1, $2, $3, $4);",
        user_id,
        username,
        identity.full_name,
        identity.avatar_url,
    )
    .execute(&mut *conn)
    .await?;

    sqlx::query!(
        "insert into tml_switchboard.user_identities \
         (provider, provider_user_id, user_id, provider_login) \
         values ($1, $2, $3, $4);",
        provider,
        identity.provider_user_id,
        user_id,
        identity.login,
    )
    .execute(&mut *conn)
    .await?;

    Ok(user_id)
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
/// of org ids they currently belong to. Only ever touches `source = 'github_org'`
/// rows, so manual memberships are preserved. See `OAUTH_LOGIN_PLAN.md` §9.3.
async fn reconcile_auto_groups(
    conn: &mut PgConnection,
    user_id: Uuid,
    provider: &str,
    org_ids: &[String],
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        with desired as (
            select gas.group_id, gas.external_id as source_ref
            from tml_switchboard.group_auto_sources gas
            where gas.provider = $2
              and gas.external_id = any($3::text[])
        ),
        removed as (
            delete from tml_switchboard.group_members gm
            where gm.member_id = $1
              and gm.source = 'github_org'
              and not exists (
                  select 1 from desired d
                  where d.group_id = gm.group_id and d.source_ref = gm.source_ref
              )
            returning 1
        )
        insert into tml_switchboard.group_members (group_id, member_id, source, source_ref)
        select d.group_id, $1, 'github_org', d.source_ref
        from desired d
        on conflict (group_id, member_id, source, source_ref) do nothing
        "#,
        user_id,
        provider,
        org_ids,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

//! DB-level tests for the login-time email re-sync: on every login the
//! provider's current email report is reconciled into `user_emails`, scoped to
//! that provider's rows — new addresses recorded, the `verified` flag aligned
//! in both directions, vanished addresses removed, and other users' (or other
//! providers') rows left untouched.
//!
//! Queries use sqlx's runtime API (not the `query!` macros), so the tests need
//! no entry in the offline `.sqlx` cache.

use std::borrow::Cow;

use sqlx::PgPool;
use uuid::Uuid;

use treadmill_switchboard::auth::oauth::{Email, ExternalIdentity};
use treadmill_switchboard::sql::user::{
    ResolveKind, create_and_reconcile, provision_existing_user, resolve_user,
};

fn identity(provider_user_id: &str, emails: &[(&str, bool)]) -> ExternalIdentity {
    ExternalIdentity {
        provider_user_id: provider_user_id.to_string(),
        login: format!("login-{provider_user_id}"),
        full_name: None,
        avatar_url: None,
        emails: emails
            .iter()
            .map(|(address, verified)| Email {
                address: Cow::Owned(address.to_string()),
                verified: *verified,
            })
            .collect(),
    }
}

/// Provision a fresh user from `identity` (as the login flow would after
/// admission + ToS acceptance).
async fn create_user(pool: &PgPool, identity: &ExternalIdentity) -> anyhow::Result<Uuid> {
    let mut tx = pool.begin().await?;
    let user_id = create_and_reconcile(&mut tx, "github", identity, &[], 1).await?;
    tx.commit().await?;
    Ok(user_id)
}

/// Re-run the login refresh for an existing user with a (possibly changed)
/// email report.
async fn refresh_user(
    pool: &PgPool,
    user_id: Uuid,
    identity: &ExternalIdentity,
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    provision_existing_user(
        &mut tx,
        "github",
        identity,
        &[],
        user_id,
        ResolveKind::IdentityMatch,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// The `(email, verified)` rows recorded for a user, ordered by address.
async fn emails_of(pool: &PgPool, user_id: Uuid) -> anyhow::Result<Vec<(String, bool)>> {
    Ok(sqlx::query_as(
        "select email, verified from tml_switchboard.user_emails \
         where user_id = $1 order by email",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?)
}

/// How many audit events of `event_type` exist.
async fn event_count(pool: &PgPool, event_type: &str) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(
        "select count(*) from tml_switchboard.audit_events where event_type = $1",
    )
    .bind(format!("{event_type}.v1"))
    .fetch_one(pool)
    .await?)
}

#[sqlx::test]
#[ignore = "requires a database; run via the nextest-db check"]
async fn login_resyncs_emails_against_the_provider_report(pool: PgPool) -> anyhow::Result<()> {
    let user = create_user(
        &pool,
        &identity("1", &[("a@example.test", true), ("b@example.test", false)]),
    )
    .await?;
    assert_eq!(
        emails_of(&pool, user).await?,
        vec![
            ("a@example.test".to_string(), true),
            ("b@example.test".to_string(), false),
        ],
    );

    // Next login: `a` is no longer verified, `b` vanished, `c` is new and
    // verified.
    refresh_user(
        &pool,
        user,
        &identity("1", &[("a@example.test", false), ("c@example.test", true)]),
    )
    .await?;
    assert_eq!(
        emails_of(&pool, user).await?,
        vec![
            ("a@example.test".to_string(), false),
            ("c@example.test".to_string(), true),
        ],
    );
    assert_eq!(
        event_count(&pool, "user_email_verification_changed").await?,
        1
    );
    assert_eq!(event_count(&pool, "user_email_removed").await?, 1);

    // `a` becomes verified again upstream: the flag flips back on file.
    refresh_user(
        &pool,
        user,
        &identity("1", &[("a@example.test", true), ("c@example.test", true)]),
    )
    .await?;
    assert_eq!(
        emails_of(&pool, user).await?,
        vec![
            ("a@example.test".to_string(), true),
            ("c@example.test".to_string(), true),
        ],
    );
    assert_eq!(
        event_count(&pool, "user_email_verification_changed").await?,
        2
    );

    // An unchanged report is a no-op: no further email events.
    refresh_user(
        &pool,
        user,
        &identity("1", &[("a@example.test", true), ("c@example.test", true)]),
    )
    .await?;
    assert_eq!(event_count(&pool, "user_email_added").await?, 3);
    assert_eq!(
        event_count(&pool, "user_email_verification_changed").await?,
        2
    );
    assert_eq!(event_count(&pool, "user_email_removed").await?, 1);

    Ok(())
}

#[sqlx::test]
#[ignore = "requires a database; run via the nextest-db check"]
async fn resync_never_touches_another_users_address(pool: PgPool) -> anyhow::Result<()> {
    let owner = create_user(&pool, &identity("1", &[("shared@example.test", true)])).await?;

    // A second, unrelated identity also reports the address. It must neither
    // steal the row nor log an event — and because the address is verified on
    // file, the resolver would link the login instead of creating a user, so
    // provision the second user with a disjoint report first.
    let mut conn = pool.acquire().await?;
    let second_identity = identity("2", &[("shared@example.test", false)]);
    assert_eq!(
        resolve_user(
            &mut conn,
            "github",
            &identity("2", &[("shared@example.test", true)])
        )
        .await?
        .map(|(id, kind)| (id, kind == ResolveKind::EmailLink)),
        Some((owner, true)),
        "a verified shared address links instead of registering",
    );
    drop(conn);

    let intruder = create_user(&pool, &second_identity).await?;
    assert_eq!(
        emails_of(&pool, owner).await?,
        vec![("shared@example.test".to_string(), true)],
        "the address stays with its original owner",
    );
    assert_eq!(emails_of(&pool, intruder).await?, vec![]);

    // The owner's next login no longer reports the address: it is removed —
    // and the intruder's identical report still cannot claim it away from a
    // later re-add by the owner.
    refresh_user(&pool, owner, &identity("1", &[])).await?;
    assert_eq!(emails_of(&pool, owner).await?, vec![]);

    Ok(())
}

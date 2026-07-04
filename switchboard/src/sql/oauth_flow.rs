//! Persistence for in-flight OAuth authorization-code flows (CSRF state).

use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

/// Persist a new flow's CSRF `state` so the callback can confirm it corresponds
/// to a flow this server initiated.
pub async fn insert_flow(
    conn: impl PgExecutor<'_>,
    state: &str,
    provider: &str,
    expires_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "insert into tml_switchboard.oauth_flows (state, provider, expires_at) \
         values ($1, $2, $3);",
        state,
        provider,
        expires_at,
    )
    .execute(conn)
    .await
    .map(|_| ())
}

/// Consume (delete) the flow identified by `state`, returning its provider iff
/// the row existed and has not expired. Unknown or expired states yield `None`
/// (an expired row is still deleted, sweeping it).
pub async fn consume_flow(
    conn: impl PgExecutor<'_>,
    state: &str,
) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query!(
        "delete from tml_switchboard.oauth_flows where state = $1 \
         returning provider, expires_at;",
        state,
    )
    .fetch_optional(conn)
    .await?;

    Ok(row.and_then(|r| (r.expires_at > Utc::now()).then_some(r.provider)))
}

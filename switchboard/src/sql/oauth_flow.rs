//! Persistence for in-flight OAuth authorization-code flows (CSRF state).

use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

/// A consumed flow: what the login route recorded when it started the flow.
#[derive(Debug, Clone)]
pub struct ConsumedFlow {
    pub provider: String,
    /// The browser frontend's declared (allowlist-validated) return URL, or
    /// `None` for a programmatic flow.
    pub return_to: Option<String>,
}

/// Persist a new flow's CSRF `state` so the callback can confirm it corresponds
/// to a flow this server initiated. `return_to` (already validated against the
/// allowlist) rides along so the callback knows where to send the browser.
pub async fn insert_flow(
    conn: impl PgExecutor<'_>,
    state: &str,
    provider: &str,
    return_to: Option<&str>,
    expires_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "insert into tml_switchboard.oauth_flows (state, provider, return_to, expires_at) \
         values ($1, $2, $3, $4);",
        state,
        provider,
        return_to,
        expires_at,
    )
    .execute(conn)
    .await
    .map(|_| ())
}

/// Consume (delete) the flow identified by `state`, returning it iff the row
/// existed and has not expired. Unknown or expired states yield `None` (an
/// expired row is still deleted, sweeping it).
pub async fn consume_flow(
    conn: impl PgExecutor<'_>,
    state: &str,
) -> Result<Option<ConsumedFlow>, sqlx::Error> {
    let row = sqlx::query!(
        "delete from tml_switchboard.oauth_flows where state = $1 \
         returning provider, return_to, expires_at;",
        state,
    )
    .fetch_optional(conn)
    .await?;

    Ok(row.and_then(|r| {
        (r.expires_at > Utc::now()).then_some(ConsumedFlow {
            provider: r.provider,
            return_to: r.return_to,
        })
    }))
}

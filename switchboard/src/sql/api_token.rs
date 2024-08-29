use crate::auth::token::SecurityToken;
use chrono::{DateTime, Utc};
use sqlx::PgExecutor;
use std::fmt::{Display, Formatter};
use thiserror::Error;
use uuid::Uuid;

/// Errors that may occur while trying to work with API tokens.
#[derive(Debug, Error)]
pub enum TokenError {
    /// A supplied token was invalid.
    #[error("invalid token")]
    InvalidToken,
    /// An error occurred in the database (this does NOT include e.g. situations where a row is
    /// expected but not found; rather, it covers situations where the database failed on its own).
    #[error("database error: {0}")]
    Database(sqlx::Error),
}

/// Corresponds to the `cancellation` type in the database schema.
///
/// Represents the time and reason for the revocation of an API token.
#[derive(Debug, Clone, sqlx::Type)]
#[sqlx(type_name = "api_token_cancellation")]
pub struct Cancellation {
    pub canceled_at: DateTime<Utc>,
    pub cancellation_reason: String,
}
impl Display for Cancellation {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "canceled at `{}` due to `{}`",
            self.canceled_at, self.cancellation_reason
        )
    }
}

/// Minimal information about a token (does not include actual token).
#[derive(Debug, Clone)]
pub struct SqlApiTokenMetadata {
    /// The token ID (which is NOT the secret 128-byte token itself, but rather the label of that
    /// token within the system).
    pub token_id: Uuid,
    /// If this is `Some`, then the token was at some prior time revoked; the information inside the
    /// [`Cancellation`] describes the precise time and circumstance.
    pub canceled: Option<Cancellation>,
    /// The time at which the token expires/expired.
    pub expires_at: DateTime<Utc>,
    /// ID of the user that owns the token
    pub user_id: Uuid,
    /// Whether the token inherits all the user's permissions or not
    pub inherits_user_permissions: bool,
}

/// Look up a token by its secret in the database.
pub async fn fetch_metadata_by_token<'c, E: PgExecutor<'c>>(
    conn: E,
    token: SecurityToken,
) -> Result<SqlApiTokenMetadata, TokenError> {
    sqlx::query_as!(
        SqlApiTokenMetadata,
        r#"SELECT token_id, user_id, inherits_user_permissions, canceled as "canceled: _",
                  expires_at
            FROM tml_switchboard.api_tokens
            WHERE token = $1
            LIMIT 1;"#,
        token.as_bytes(),
    )
    .fetch_one(conn)
    .await
    .map_err(|e| match e {
        sqlx::Error::RowNotFound => TokenError::InvalidToken,
        e => TokenError::Database(e),
    })
}

pub async fn fetch_metadata_by_id<'c, E: PgExecutor<'c>>(
    conn: E,
    token_id: Uuid,
) -> Result<SqlApiTokenMetadata, TokenError> {
    sqlx::query_as!(
        SqlApiTokenMetadata,
        r#"SELECT token_id, user_id, inherits_user_permissions, canceled as "canceled: _",
                  expires_at
            FROM tml_switchboard.api_tokens
            WHERE token_id = $1
            LIMIT 1;"#,
        token_id
    )
    .fetch_one(conn)
    .await
    .map_err(|e| match e {
        sqlx::Error::RowNotFound => TokenError::InvalidToken,
        e => TokenError::Database(e),
    })
}

pub enum TokenPerms {
    HasOwn,
    InheritsUserPerms,
}

pub async fn insert_token(
    conn: impl PgExecutor<'_>,
    user_id: Uuid,
    lifetime: chrono::TimeDelta,
    inherits_user_perms: TokenPerms,
) -> Result<(SecurityToken, DateTime<Utc>), sqlx::Error> {
    let token_id = Uuid::new_v4();
    let api_token = SecurityToken::generate();
    let created = Utc::now();
    let expires = created + lifetime;
    sqlx::query!(
        "insert into tml_switchboard.api_tokens values ($1, $2, $3, $4, null, $5, $6);",
        token_id,
        api_token.as_bytes(),
        user_id,
        matches!(inherits_user_perms, TokenPerms::InheritsUserPerms),
        created,
        expires
    )
    .execute(conn)
    .await
    .map(|_| (api_token, expires))
}

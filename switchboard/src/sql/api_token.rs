use crate::audit::model::Subject as AuditSubject;
use crate::audit::{Transition, WriteToken, events};
use crate::auth::token::SecurityToken;
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgExecutor};
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

/// Corresponds to the `revocation` type in the database schema.
///
/// Represents the time and reason for the revocation of an API token.
#[derive(Debug, Clone, sqlx::Type)]
#[sqlx(type_name = "tml_switchboard.api_token_revocation")]
pub struct Revocation {
    pub revoked_at: DateTime<Utc>,
    pub revocation_reason: String,
}
impl Display for Revocation {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "revoked at `{}` due to `{}`",
            self.revoked_at, self.revocation_reason
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
    /// [`Revocation`] describes the precise time and circumstance.
    pub revoked: Option<Revocation>,
    /// The time at which the token expires/expired.
    pub expires_at: DateTime<Utc>,
    /// ID of the user that owns the token
    pub user_id: Uuid,
    /// Whether the owning user's account is locked. A locked account's tokens
    /// are rejected at extraction time, so a since-locked user's existing
    /// sessions stop working immediately.
    pub locked: bool,
}

/// Look up a token by its secret in the database.
pub async fn fetch_metadata_by_token<'c, E: PgExecutor<'c>>(
    conn: E,
    token: SecurityToken,
) -> Result<SqlApiTokenMetadata, TokenError> {
    sqlx::query_as!(
        SqlApiTokenMetadata,
        r#"SELECT t.token_id, t.user_id, t.revoked as "revoked: _",
                  t.expires_at, u.locked
            FROM tml_switchboard.api_tokens t
            JOIN tml_switchboard.users u ON u.subject_id = t.user_id
            WHERE t.token = $1
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
        r#"SELECT t.token_id, t.user_id, t.revoked as "revoked: _",
                  t.expires_at, u.locked
            FROM tml_switchboard.api_tokens t
            JOIN tml_switchboard.users u ON u.subject_id = t.user_id
            WHERE t.token_id = $1
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

/// Mint a session/API token for a user, recording its provenance (the client
/// address and user agent that requested it, plus an optional human comment).
/// Emits [`events::SessionTokenIssued`]. Run via [`audit::transition`].
///
/// [`audit::transition`]: crate::audit::transition
pub struct IssueSessionToken {
    pub user_id: Uuid,
    pub lifetime: chrono::TimeDelta,
    pub user_agent: Option<String>,
    pub comment: Option<String>,
    pub created_ip: Option<String>,
    pub created_port: Option<i32>,
}

impl Transition for IssueSessionToken {
    type Output = (SecurityToken, DateTime<Utc>);
    type Event = events::SessionTokenIssued;

    async fn apply(
        self,
        conn: &mut PgConnection,
        _w: &WriteToken,
    ) -> Result<(Self::Output, Self::Event), sqlx::Error> {
        // Time-ordered (v7) for primary-key insert locality. This is only the
        // token's identifier; the token *secret* is the separate 128-byte
        // `SecurityToken` generated below, so v7's embedded timestamp leaks
        // nothing sensitive.
        let token_id = Uuid::now_v7();
        let api_token = SecurityToken::generate();
        let created = Utc::now();
        let expires = created + self.lifetime;
        sqlx::query!(
            "insert into tml_switchboard.api_tokens \
             (token_id, token, user_id, revoked, created_at, expires_at, user_agent, comment, created_ip, created_port) \
             values ($1, $2, $3, null, $4, $5, $6, $7, $8, $9);",
            token_id,
            api_token.as_bytes(),
            self.user_id,
            created,
            expires,
            self.user_agent,
            self.comment,
            self.created_ip,
            self.created_port,
        )
        .execute(&mut *conn)
        .await?;

        let event = events::SessionTokenIssued {
            actor: AuditSubject(self.user_id),
            user: AuditSubject(self.user_id),
            token_id,
            expires_at: expires,
            client_ip: self.created_ip,
            client_port: self.created_port,
            user_agent: self.user_agent,
        };
        Ok(((api_token, expires), event))
    }
}

/// Revoke (cancel) a token. The route confirms the token belongs to the caller
/// before running this. Idempotent on the write (only flips an un-revoked
/// token) but always emits [`events::SessionTokenRevoked`]. Run via
/// [`audit::transition`](crate::audit::transition).
pub struct RevokeToken {
    pub user_id: Uuid,
    pub token_id: Uuid,
    pub reason: String,
}

impl Transition for RevokeToken {
    type Output = ();
    type Event = events::SessionTokenRevoked;

    async fn apply(
        self,
        conn: &mut PgConnection,
        _w: &WriteToken,
    ) -> Result<(Self::Output, Self::Event), sqlx::Error> {
        sqlx::query!(
            "update tml_switchboard.api_tokens \
             set revoked = row(now(), $2)::tml_switchboard.api_token_revocation \
             where token_id = $1 and revoked is null;",
            self.token_id,
            self.reason,
        )
        .execute(&mut *conn)
        .await?;

        let event = events::SessionTokenRevoked {
            actor: AuditSubject(self.user_id),
            user: AuditSubject(self.user_id),
            token_id: self.token_id,
        };
        Ok(((), event))
    }
}

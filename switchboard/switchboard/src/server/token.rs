//! Facilities for managing API tokens.

use base64::prelude::BASE64_STANDARD_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Utc};
use headers::authorization::Bearer;
use serde::{Deserialize, Serialize};
use sqlx::PgExecutor;
use std::fmt::{Display, Formatter};
use subtle::{Choice, ConstantTimeEq};
use thiserror::Error;
use treadmill_rs::api::switchboard::AuthToken;
use uuid::Uuid;

/// An API token is a 128-byte string generated by a cryptographically secure PRNG.
///
/// On the wire, it is represented as a standard, unpadded base-64 string.
///
/// Equality testing is provided via [`Eq`] and [`PartialEq`], and implemented with
/// [`subtle::ConstantTimeEq`] to mitigate timing attacks.
// Use `serde_with::serde_as` since `serde` by itself doesn't support arrays larger than 32 items,
// and also because `serde_with` has builtin base64-encoding support.
#[derive(Debug, Serialize, Deserialize, Eq, PartialEq, Copy, Clone)]
#[serde(transparent)]
pub struct ApiToken(AuthToken);
impl Into<AuthToken> for ApiToken {
    fn into(self) -> AuthToken {
        self.0
    }
}
impl ApiToken {
    /// Generate a random new ApiToken.
    ///
    /// Since this is a bit more than 8 times wider than a v4 UUID (122 bits), collisions are next
    /// to impossible.
    ///
    /// Uses [`rand::ThreadRng`] under the hood.
    pub fn generate() -> Self {
        Self(AuthToken(rand::random()))
    }

    /// Get access to an immutable byte representation of the token.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0 .0
    }
}
impl ConstantTimeEq for ApiToken {
    fn ct_eq(&self, other: &Self) -> Choice {
        self.0.ct_eq(&other.0)
    }
}
impl TryFrom<Vec<u8>> for ApiToken {
    type Error = <[u8; 32] as TryFrom<Vec<u8>>>::Error;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        Ok(Self(AuthToken(value.try_into()?)))
    }
}
impl TryFrom<Bearer> for ApiToken {
    type Error = serde_json::Error;

    fn try_from(value: Bearer) -> Result<Self, Self::Error> {
        serde_json::from_str(value.token())
    }
}
impl Display for ApiToken {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&BASE64_STANDARD_NO_PAD.encode(&self.0 .0))
    }
}

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

/// Minimal information about a token.
#[derive(Debug, Clone)]
pub struct TokenInfoBare {
    /// The token ID (which is NOT the secret 128-byte token itself, but rather the label of that
    /// token within the system).
    pub token_id: Uuid,
    /// If this is `Some`, then the token was at some prior time revoked; the information inside the
    /// [`Cancellation`] describes the precise time and circumstance.
    pub canceled: Option<Cancellation>,
    /// The time at which the token expires/expiree.
    pub expires_at: DateTime<Utc>,
    /// ID of the user that owns the token
    pub user_id: Uuid,
    /// Whether the token inherits all the user's permissions or not
    pub inherits_user_permissions: bool,
}
impl TokenInfoBare {
    /// Look up a token by its secret in the database.
    pub async fn lookup<'c, E: PgExecutor<'c>>(
        conn: E,
        token: ApiToken,
    ) -> Result<TokenInfoBare, TokenError> {
        sqlx::query_as!(
            TokenInfoBare,
            // need `as "canceled: _"` for weird SQLx reasons
            r#"SELECT token_id, user_id, inherits_user_permissions, canceled as "canceled: _",
                  expires_at
            FROM api_tokens
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
}

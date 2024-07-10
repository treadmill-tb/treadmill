use base64::prelude::BASE64_STANDARD_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Utc};
use headers::Header;
use http::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_with::base64::Base64;
use serde_with::serde_as;
use sqlx::PgExecutor;
use std::fmt::{Display, Formatter};
use subtle::ConstantTimeEq;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct XApiToken(pub ApiToken);
impl From<XApiToken> for ApiToken {
    fn from(value: XApiToken) -> Self {
        value.0
    }
}
impl From<ApiToken> for XApiToken {
    fn from(value: ApiToken) -> Self {
        XApiToken(value)
    }
}
impl Header for XApiToken {
    fn name() -> &'static HeaderName {
        static HEADER_NAME: HeaderName = HeaderName::from_static("x-api-token");
        &HEADER_NAME
    }
    fn decode<'i, I>(values: &mut I) -> Result<Self, headers::Error>
    where
        Self: Sized,
        I: Iterator<Item = &'i HeaderValue>,
    {
        let value = values.next().ok_or_else(headers::Error::invalid)?;
        let csrf_token: ApiToken =
            serde_json::from_str(value.to_str().map_err(|_| headers::Error::invalid())?)
                .map_err(|_| headers::Error::invalid())?;
        Ok(Self(csrf_token))
    }
    fn encode<E: Extend<HeaderValue>>(&self, values: &mut E) {
        values.extend(std::iter::once(
            HeaderValue::from_str(serde_json::to_string(&self.0).unwrap().as_str()).unwrap(),
        ));
    }
}

#[serde_as]
#[derive(Debug, Serialize, Deserialize, Eq, Copy, Clone)]
pub struct ApiToken(#[serde_as(as = "Base64")] [u8; 128]);
impl ApiToken {
    pub fn generate() -> Self {
        Self(rand::random())
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}
impl PartialEq for ApiToken {
    fn eq(&self, other: &Self) -> bool {
        // use ConstantTimeEq to mitigate timing attacks
        self.0.ct_eq(&other.0).into()
    }
}
impl TryFrom<Vec<u8>> for ApiToken {
    type Error = Vec<u8>;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        Ok(Self(value.try_into()?))
    }
}
impl Display for ApiToken {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&BASE64_STANDARD_NO_PAD.encode(&self.0))
    }
}

#[derive(Debug, Error)]
pub enum TokenError {
    #[error("invalid token")]
    InvalidToken,
    #[error("database error: {0}")]
    Database(sqlx::Error),
}
#[derive(Debug, sqlx::Type)]
#[sqlx(type_name = "cancellation")]
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
#[derive(Debug)]
pub struct TokenInfoBare {
    pub token_id: Uuid,
    pub canceled: Option<Cancellation>,
    pub expires_at: DateTime<Utc>,
}

pub async fn token_lookup<'c, E: PgExecutor<'c>>(
    conn: E,
    token: ApiToken,
) -> Result<TokenInfoBare, TokenError> {
    sqlx::query_as!(
        TokenInfoBare,
        r#"SELECT token_id, canceled as "canceled: _", expires_at FROM api_tokens WHERE token = $1 LIMIT 1;"#,
        &token.0,
    )
    .fetch_one(conn)
    .await
    .map_err(|e| match e {
        sqlx::Error::RowNotFound => TokenError::InvalidToken,
        e => TokenError::Database(e),
    })
}

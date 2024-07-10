use crate::server::AppState;
use argon2::password_hash::{PasswordHashString, Salt, SaltString};
use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version};
use axum::extract::{ConnectInfo, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use axum_extra::extract::cookie::{Cookie, SameSite};
use axum_extra::extract::SignedCookieJar;
use axum_extra::TypedHeader;
use base64::prelude::{BASE64_STANDARD, BASE64_STANDARD_NO_PAD};
use base64::Engine;
use chrono::{DateTime, Utc};
use futures_util::TryFutureExt;
use headers::{Header, UserAgent};
use http::{HeaderName, HeaderValue, StatusCode};
use serde::{Deserialize, Serialize};
use serde_with::base64::Base64;
use serde_with::serde_as;
use sqlx::encode::IsNull;
use sqlx::error::BoxDynError;
use sqlx::types::ipnetwork::IpNetwork;
use sqlx::{Database, Encode, Error, Executor, PgExecutor, Postgres, Type};
use std::fmt::{Display, Formatter};
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::LazyLock;
use subtle::ConstantTimeEq;
use thiserror::Error;
use uuid::Uuid;

pub static SESSION_ID_COOKIE: &str = "__Host-TML-Session-Id";
pub static PRESESSION_ID_COOKIE: &str = "__Host-TML-Presession-Id";

#[derive(Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct XCsrfToken(pub CsrfToken);
impl From<XCsrfToken> for CsrfToken {
    fn from(value: XCsrfToken) -> Self {
        value.0
    }
}
impl From<CsrfToken> for XCsrfToken {
    fn from(value: CsrfToken) -> Self {
        XCsrfToken(value)
    }
}
impl Header for XCsrfToken {
    fn name() -> &'static HeaderName {
        static HEADER_NAME: HeaderName = HeaderName::from_static("x-csrf-token");
        &HEADER_NAME
    }
    fn decode<'i, I>(values: &mut I) -> Result<Self, headers::Error>
    where
        Self: Sized,
        I: Iterator<Item = &'i HeaderValue>,
    {
        let value = values.next().ok_or_else(headers::Error::invalid)?;
        let csrf_token: CsrfToken =
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
impl Display for CsrfToken {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&BASE64_STANDARD_NO_PAD.encode(&self.0))
    }
}

// TODO: SessionId

#[serde_as]
#[derive(Debug, Serialize, Deserialize, Eq, Copy, Clone)]
pub struct CsrfToken(#[serde_as(as = "Base64")] [u8; 128]);
impl PartialEq for CsrfToken {
    fn eq(&self, other: &Self) -> bool {
        // use ConstantTimeEq to mitigate timing attacks
        self.0.ct_eq(&other.0).into()
    }
}
impl TryFrom<Vec<u8>> for CsrfToken {
    type Error = Vec<u8>;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        Ok(Self(value.try_into()?))
    }
}

pub async fn presession_handler(
    TypedHeader(user_agent): TypedHeader<UserAgent>,
    ConnectInfo(socket_addr): ConnectInfo<SocketAddr>,
    mut signed_cookie_jar: SignedCookieJar,
    State(app_state): State<AppState>,
) -> Response {
    tracing::info!("/session/presession");

    if let Some(preexisting_session) = signed_cookie_jar.get(SESSION_ID_COOKIE) {
        // TODO: should we not log the cookie value?
        tracing::warn!(
            "requested presession with preexisting session cookie: {preexisting_session}",
        );
        return StatusCode::BAD_REQUEST.into_response();
    }

    if let Some(preexisting_presession) = signed_cookie_jar.get(PRESESSION_ID_COOKIE) {
        tracing::warn!(
            "requested presession with preexisting presession cookie: {preexisting_presession}",
        );
        // get rid of prior presession
        match serde_json::from_str::<Uuid>(preexisting_presession.value()) {
            Ok(presession_id) => {
                match presession_destroy(&app_state.db_pool, presession_id).await {
                    Ok(()) => (),
                    Err(SessionError::InvalidPresession) => {
                        tracing::warn!("no such presession in database");
                    }
                    Err(e) => {
                        tracing::error!("failed to destroy presession: {e}");
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                }
            }
            Err(e) => {
                tracing::warn!("failed to deserialize presession cookie: {e}");
            }
        }
    }

    let presession = match presession_create(
        &app_state.db_pool,
        user_agent,
        socket_addr.ip(),
        chrono::Duration::from_std(app_state.config.api.auth_presession_timeout).unwrap(),
    )
    .await
    {
        Ok(info) => info,
        Err(e) => {
            tracing::error!("Failed to register presession for: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let uuid_str = serde_json::to_string(&presession.presession_id)
        .expect("Failed to serialize presession ID");

    // Note that it's better NOT to set Domain for this, since we already have it as a __Host-
    // cookie.
    let mut cookie = Cookie::new(PRESESSION_ID_COOKIE, uuid_str);
    cookie.set_same_site(Some(SameSite::Strict));
    // TODO: SSL
    //cookie.set_secure(Some(true));
    cookie.set_http_only(Some(true));
    // Max-Age is more precedent than Expires
    cookie.set_max_age(Some(
        time::Duration::try_from(app_state.config.api.auth_presession_timeout).unwrap(),
    ));
    signed_cookie_jar = signed_cookie_jar.add(cookie);

    (
        StatusCode::OK,
        signed_cookie_jar,
        TypedHeader(XCsrfToken(presession.csrf_token)),
    )
        .into_response()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LoginRequest {
    username_or_email: String,
    password: String,
}

/// Returns:
///     200 OK          authentication succeeded
///     403 FORBIDDEN   invalid username or password
///     400 BAD REQUEST malformed login request, CSRF failure, etc.
///     500 I.S.E.      internal error
/// In all cases, the response body is empty.
pub async fn login_handler(
    TypedHeader(user_agent): TypedHeader<UserAgent>,
    TypedHeader(x_csrf_token): TypedHeader<XCsrfToken>,
    ConnectInfo(socket_addr): ConnectInfo<SocketAddr>,
    mut signed_cookie_jar: SignedCookieJar,
    State(app_state): State<AppState>,
    Json(login_request): Json<LoginRequest>,
) -> Response {
    tracing::info!("/session/login");

    if let Some(preexisting_session) = signed_cookie_jar.get(SESSION_ID_COOKIE) {
        // TODO: should we not log the cookie value?
        tracing::warn!("tried to log in with preexisting session cookie: {preexisting_session}");
        return StatusCode::BAD_REQUEST.into_response();
    }

    let Some(presession_cookie) = signed_cookie_jar.get(PRESESSION_ID_COOKIE) else {
        tracing::warn!("tried to log in without presession cookie");
        return StatusCode::BAD_REQUEST.into_response();
    };

    let presession_id: Uuid = match serde_json::from_str(presession_cookie.value()) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!("failed to deserialize presession cookie: {e}");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    let presession = match presession_lookup(&app_state.db_pool, presession_id)
        .await
        .and_then(|presession| {
            if presession.expires_at >= Utc::now() {
                Ok(presession)
            } else {
                Err(SessionError::ExpiredPresession)
            }
        }) {
        Ok(presession) => presession,
        Err(e) => {
            tracing::warn!("failed to look up presession: {e}");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    if presession.csrf_token != x_csrf_token.0 {
        tracing::warn!(
            "mismatched CSRF tokens for presession ID ({presession_id}); correct ({}), got ({})!",
            presession.presession_id,
            x_csrf_token.0,
        );

        return StatusCode::BAD_REQUEST.into_response();
    }

    if let Err(e) = presession_destroy(&app_state.db_pool, presession_id).await {
        tracing::error!("failed to destroy presession: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // CSRF is okay, check credentials

    let argon2 = Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(19456, 2, 1, Some(64)).unwrap(),
    );

    static LOGIN_FAILED_RESPONSE: (StatusCode, &str) =
        (StatusCode::FORBIDDEN, "invalid user ID or password");

    let user_lookup_result =
        lookup_user_password_hash(&app_state.db_pool, &login_request.username_or_email).await;
    let user = match user_lookup_result {
        Ok(user) => {
            match argon2.verify_password(
                login_request.password.as_bytes(),
                &user.password_hash.password_hash(),
            ) {
                Ok(()) => {
                    if user.locked {
                        tracing::info!("failed login attempt to user ({}): locked", user.user_id);
                        return LOGIN_FAILED_RESPONSE.into_response();
                    } else {
                        tracing::info!("user ({}) logged in successfully", user.user_id);
                        user
                    }
                }
                Err(argon2::password_hash::Error::Password) => {
                    tracing::info!(
                        "failed login attempt to user ({}): wrong password",
                        user.user_id
                    );
                    return LOGIN_FAILED_RESPONSE.into_response();
                }
                Err(e) => {
                    tracing::error!("failed to verify password for user ({}): {e}", user.user_id);
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            }
        }
        Err(SessionError::InvalidUsername(user)) => {
            static FAKE_SALT: LazyLock<SaltString> =
                LazyLock::new(|| SaltString::from_b64("A123B123C123D123").unwrap());

            let _ = std::hint::black_box(
                argon2.hash_password(login_request.password.as_bytes(), FAKE_SALT.as_salt()),
            );

            tracing::error!(
                "user identified by user ID `{}` does not exist",
                login_request.username_or_email
            );
            return LOGIN_FAILED_RESPONSE.into_response();
        }
        Err(e) => {
            tracing::error!(
                "failed to look up credentials for user ID `{}`: {e}",
                login_request.username_or_email
            );
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // okay, login was successful, create a session

    let UserPresessionRecord {
        presession_id: _,
        csrf_token: _,
        user_agent,
        remote_ip,
        created_at: _,
        expires_at: _,
    } = presession;

    let session = match session_create(
        &app_state.db_pool,
        user_agent,
        remote_ip,
        chrono::Duration::from_std(app_state.config.api.auth_session_timeout).unwrap(),
        user.user_id,
    )
    .await
    {
        Ok(session) => session,
        Err(e) => {
            tracing::error!("failed to create session for user ({}): {e}", user.user_id);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let session_id_str =
        serde_json::to_string(&session.session_id).expect("Failed to serialize session ID");

    // Note that it's better NOT to set Domain for this, since we already have it as a __Host-
    // cookie.
    let mut cookie = Cookie::new(PRESESSION_ID_COOKIE, session_id_str);
    cookie.set_same_site(Some(SameSite::Strict));
    // TODO: SSL
    //cookie.set_secure(Some(true));
    cookie.set_http_only(Some(true));
    // Max-Age is more precedent than Expires
    cookie.set_max_age(Some(
        time::Duration::try_from(app_state.config.api.auth_presession_timeout).unwrap(),
    ));
    signed_cookie_jar = signed_cookie_jar.add(cookie);

    (
        StatusCode::OK,
        signed_cookie_jar,
        TypedHeader(XCsrfToken(session.csrf_token)),
    )
        .into_response()
}

// -- INTERNALS --

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("database error: {0}")]
    Database(sqlx::Error),
    #[error("invalid presession")]
    InvalidPresession,
    #[error("expired presession")]
    ExpiredPresession,
    #[error("invalid user: {0}")]
    InvalidUsername(String),
    #[error("invalid session")]
    InvalidSession,
    #[error("expired session")]
    ExpiredSession,
}

// -- Look up a user's credentials

#[derive(Debug)]
struct UserCredentialLookup {
    user_id: Uuid,
    password_hash: PasswordHashString,
    locked: bool,
}
async fn lookup_user_password_hash<'c, E: PgExecutor<'c>>(
    conn: E,
    username_or_email: &str,
) -> Result<UserCredentialLookup, SessionError> {
    struct TempRecord {
        user_id: Uuid,
        password_hash: String,
        locked: bool,
    }
    let TempRecord { user_id, password_hash, locked } = sqlx::query_as!(
        TempRecord,
        r#"SELECT user_id, password_hash, locked FROM users WHERE name = $1 OR email = $1 LIMIT 1;"#,
    username_or_email)
        .fetch_one(conn)
        .await
        .map_err(|e| match e {
            Error::RowNotFound => SessionError::InvalidUsername(username_or_email.to_string()),
            e => SessionError::Database(e),
        })?;
    let password_hash =
        PasswordHashString::new(&password_hash).expect("failed to parse password hash in database");
    Ok(UserCredentialLookup {
        user_id,
        password_hash,
        locked,
    })
}

// -- Functiosn for working with sessions

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserSessionRecord {
    pub session_id: Uuid,
    #[sqlx(try_from = "Vec<u8>")]
    pub csrf_token: CsrfToken,
    pub user_agent: String,
    pub remote_ip: IpAddr,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub user_id: Uuid,
}

async fn session_destroy<'c, E: PgExecutor<'c>>(
    conn: E,
    session_id: Uuid,
) -> Result<(), SessionError> {
    let _ = sqlx::query!(
        r#"DELETE FROM "user_sessions" WHERE "session_id"=$1;"#,
        session_id
    )
    .execute(conn)
    .await
    .map_err(|e| match e {
        Error::RowNotFound => SessionError::InvalidSession,
        e => SessionError::Database(e),
    })?;
    Ok(())
}

pub async fn session_lookup<'c, E: PgExecutor<'c>>(
    conn: E,
    presession_id: Uuid,
) -> Result<UserSessionRecord, SessionError> {
    struct TempRecord {
        session_id: Uuid,
        csrf_token: Vec<u8>,
        user_agent: String,
        remote_ip: IpNetwork,
        created_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
        user_id: Uuid,
    }
    sqlx::query_as!(
        TempRecord,
        r#"SELECT * FROM "user_sessions" WHERE "session_id"=$1 LIMIT 1;"#,
        presession_id,
    )
    .fetch_one(conn)
    .await
    .map_err(|e| match e {
        Error::RowNotFound => SessionError::InvalidPresession,
        e => SessionError::Database(e),
    })
    .map(
        |TempRecord {
             session_id,
             csrf_token,
             user_agent,
             remote_ip,
             created_at,
             expires_at,
             user_id,
         }| {
            let token_barr: [u8; 128] = csrf_token
                .try_into()
                .expect("stored     token has wrong length");
            UserSessionRecord {
                session_id,
                csrf_token: CsrfToken(token_barr),
                user_agent,
                remote_ip: remote_ip.ip(),
                created_at,
                expires_at,
                user_id,
            }
        },
    )
}

async fn session_create<'c, E: PgExecutor<'c>>(
    conn: E,
    user_agent: String,
    ip_addr: IpAddr,
    lifetime: chrono::Duration,
    user_id: Uuid,
) -> Result<UserSessionRecord, SessionError> {
    let session_id = Uuid::new_v4();
    let csrf_token = CsrfToken(rand::random());
    let created_at = Utc::now();
    let expires_at = created_at + lifetime;
    let record = UserSessionRecord {
        session_id,
        csrf_token,
        user_agent: user_agent.to_string(),
        remote_ip: ip_addr,
        created_at,
        expires_at,
        user_id,
    };
    let _ = sqlx::query!(
        r#"INSERT INTO "user_sessions" VALUES ($1,$2,$3,$4,$5,$6,$7);"#,
        record.session_id,
        &record.csrf_token.0,
        record.user_agent,
        IpNetwork::from(record.remote_ip),
        record.created_at,
        record.expires_at,
        record.user_id,
    )
    .execute(conn)
    .await
    .map_err(SessionError::Database)?;
    Ok(record)
}

// -- Functions for working with pre-sessions

#[derive(Debug, Clone, sqlx::FromRow)]
struct UserPresessionRecord {
    presession_id: Uuid,
    #[sqlx(try_from = "Vec<u8>")]
    csrf_token: CsrfToken,
    user_agent: String,
    remote_ip: IpAddr,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

async fn presession_destroy<'c, E: PgExecutor<'c>>(
    conn: E,
    presession_id: Uuid,
) -> Result<(), SessionError> {
    let _ = sqlx::query!(
        r#"DELETE FROM "user_presessions" WHERE "presession_id"=$1;"#,
        presession_id
    )
    .execute(conn)
    .await
    .map_err(|e| match e {
        Error::RowNotFound => SessionError::InvalidPresession,
        e => SessionError::Database(e),
    })?;
    Ok(())
}

async fn presession_lookup<'c, E: PgExecutor<'c>>(
    conn: E,
    presession_id: Uuid,
) -> Result<UserPresessionRecord, SessionError> {
    struct TempRecord {
        presession_id: Uuid,
        csrf_token: Vec<u8>,
        user_agent: String,
        remote_ip: IpNetwork,
        created_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    }
    sqlx::query_as!(
        TempRecord,
        r#"SELECT * FROM "user_presessions" WHERE "presession_id"=$1 LIMIT 1;"#,
        presession_id,
    )
    .fetch_one(conn)
    .await
    .map_err(|e| match e {
        Error::RowNotFound => SessionError::InvalidPresession,
        e => SessionError::Database(e),
    })
    .map(
        |TempRecord {
             presession_id,
             csrf_token,
             user_agent,
             remote_ip,
             created_at,
             expires_at,
         }| {
            let token_barr: [u8; 128] = csrf_token
                .try_into()
                .expect("stored token has wrong length");
            UserPresessionRecord {
                presession_id,
                csrf_token: CsrfToken(token_barr),
                user_agent,
                remote_ip: remote_ip.ip(),
                created_at,
                expires_at,
            }
        },
    )
}

async fn presession_create<'c, E: PgExecutor<'c>>(
    conn: E,
    user_agent: UserAgent,
    ip_addr: IpAddr,
    lifetime: chrono::Duration,
) -> Result<UserPresessionRecord, SessionError> {
    let presession_id = Uuid::new_v4();
    let csrf_token = CsrfToken(rand::random());
    let created_at = Utc::now();
    let expires_at = created_at + lifetime;
    let record = UserPresessionRecord {
        presession_id,
        csrf_token,
        user_agent: user_agent.to_string(),
        remote_ip: ip_addr,
        created_at,
        expires_at,
    };
    let _ = sqlx::query!(
        r#"INSERT INTO "user_presessions" VALUES ($1,$2,$3,$4,$5,$6);"#,
        record.presession_id,
        &record.csrf_token.0,
        record.user_agent,
        IpNetwork::from(record.remote_ip),
        record.created_at,
        record.expires_at,
    )
    .execute(conn)
    .await
    .map_err(SessionError::Database)?;
    Ok(record)
}

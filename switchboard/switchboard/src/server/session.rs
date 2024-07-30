//! Session management interfaces.

use crate::server::token::SecurityToken;
use crate::server::AppState;
use argon2::password_hash::{PasswordHashString, SaltString};
use argon2::{Algorithm, Argon2, Params, PasswordHasher, PasswordVerifier, Version};
use axum::extract::{ConnectInfo, State};
use axum::Json;
use chrono::{DateTime, Utc};
use http::StatusCode;
use sqlx::{Error, PgExecutor};
use std::net::SocketAddr;
use thiserror::Error;
use treadmill_rs::api::switchboard::{LoginRequest, LoginResponse};
use uuid::Uuid;

/// Handler for `/session/login`.
///
/// # Request
///
/// Expects request body to be JSON-encoded [`LoginRequest`];.
///
/// # Response
///
/// Status codes
/// ```text
///     200 OK          authentication succeeded
///     400 BAD REQUEST malformed login request, CSRF failure, etc.
///     403 FORBIDDEN   invalid username or password
///     500 I.S.E.      internal error
/// ```
///
/// Response body is JSON-encoded [`LoginResponse`] if the status code is 200, otherwise the
/// response body is empty.
pub async fn login_handler(
    ConnectInfo(_socket_addr): ConnectInfo<SocketAddr>,
    State(app_state): State<AppState>,
    Json(login_request): Json<LoginRequest>,
) -> Result<(StatusCode, Json<LoginResponse>), StatusCode> {
    tracing::info!("/session/login");

    let argon2 = Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(19456, 2, 1, Some(64)).unwrap(),
    );

    let user_lookup_result =
        lookup_user_password_hash(&app_state.db_pool, &login_request.user_identifier).await;
    let user = match user_lookup_result {
        Ok(user) => {
            match argon2.verify_password(
                login_request.password.as_bytes(),
                &user.password_hash.password_hash(),
            ) {
                Ok(()) => {
                    if user.locked {
                        tracing::info!("failed login attempt to user ({}): locked", user.user_id);
                        return Err(StatusCode::FORBIDDEN);
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
                    return Err(StatusCode::FORBIDDEN);
                }
                Err(e) => {
                    tracing::error!("failed to verify password for user ({}): {e}", user.user_id);
                    return Err(StatusCode::INTERNAL_SERVER_ERROR);
                }
            }
        }
        Err(SessionError::InvalidUsername(_)) => {
            let fake_salt = SaltString::from_b64("A123B123C123D123E123F1").unwrap();
            let _ = std::hint::black_box(
                argon2.hash_password(login_request.password.as_bytes(), fake_salt.as_salt()),
            );

            tracing::error!(
                "user identified by user ID `{}` does not exist",
                login_request.user_identifier
            );
            return Err(StatusCode::FORBIDDEN);
        }
        Err(e) => {
            tracing::error!(
                "failed to look up credentials for user ID `{}`: {e}",
                login_request.user_identifier
            );
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    // okay, login was successful, create a session token

    async fn create_token(
        conn: impl PgExecutor<'_>,
        user_id: Uuid,
        inherit_perms: bool,
        lifetime: chrono::TimeDelta,
    ) -> Result<(SecurityToken, DateTime<Utc>), sqlx::Error> {
        let token_id = Uuid::new_v4();
        let api_token = SecurityToken::generate();
        let created = Utc::now();
        let expires = created + lifetime;
        sqlx::query!(
            "insert into api_tokens values ($1, $2, $3, $4, null, $5, $6);",
            token_id,
            api_token.as_bytes(),
            user_id,
            inherit_perms,
            created,
            expires
        )
        .execute(conn)
        .await
        .map(|_| (api_token, expires))
    }

    let (token, expires_at) = create_token(
        app_state.pool(),
        user.user_id,
        true,
        app_state.config.api.auth_session_timeout,
    )
    .await
    .map_err(|e| {
        tracing::error!(
            "failed to create temporary session token for user({}): {e}",
            user.user_id
        );
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok((
        StatusCode::OK,
        Json(LoginResponse {
            token: token.into(),
            expires_at,
        }),
    ))
}

/// Things that can go wrong in session management.
#[derive(Debug, Error)]
pub enum SessionError {
    /// Database error; note that unlike the underlying [`sqlx::Error`], this does *not* include
    /// the row-not-found case.
    #[error("database error: {0}")]
    Database(sqlx::Error),
    /// Username is invalid; used for internal logging only.
    /// TODO: remove
    #[error("invalid user: {0}")]
    InvalidUsername(String),
}

// -- INTERNALS --

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

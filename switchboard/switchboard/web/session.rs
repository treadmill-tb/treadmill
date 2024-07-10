use crate::server::AppState;
use axum::extract::{ConnectInfo, FromRef, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use axum_extra::extract::cookie::{Cookie, SameSite};
use axum_extra::extract::SignedCookieJar;
use axum_extra::TypedHeader;
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use headers::UserAgent;
use http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgQueryResult;
use sqlx::types::ipnetwork::IpNetwork;
use sqlx::{Executor, Postgres};
use std::net::SocketAddr;
use uuid::Uuid;

pub static SESSION_COOKIE_NAME: &str = "TML-Session-Id";

pub static CSRF_HEADER: &str = "Sec-TML-CSRF-Token";

impl FromRef<AppState> for axum_extra::extract::cookie::Key {
    fn from_ref(input: &AppState) -> Self {
        input.cookie_signing_key.clone()
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PresessionResponse {
    presession_id: Uuid,
}

pub async fn presession_handler(
    signed_cookie_jar: SignedCookieJar,
    State(app_state): State<AppState>,
    ConnectInfo(socket_addr): ConnectInfo<SocketAddr>,
    TypedHeader(user_agent): TypedHeader<UserAgent>,
) -> Response {
    // TODO: Check origin
    tracing::info!("presession_handler");

    if let Some(preexisting_session) = signed_cookie_jar.get(SESSION_COOKIE_NAME) {
        tracing::error!(
            "{socket_addr} requested presession with set session cookie: {preexisting_session}"
        );
        return (StatusCode::FORBIDDEN,).into_response();
    }
    let presession_info = match register_presession(
        &app_state.db_pool,
        Fingerprint {
            user_agent: user_agent.to_string(),
            remote_ip: socket_addr.ip(),
        },
        Duration::from_std(app_state.config.api.auth_presession_timeout).unwrap(),
    )
    .await
    {
        Ok(psi) => psi,
        Err(e) => {
            tracing::error!("Failed to register pression for {user_agent} at {socket_addr}: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let psid_cookie = Cookie::new(
        "__Host-Presession-Id",
        base64::prelude::BASE64_STANDARD.encode(&presession_info.csrf_token),
    )
    // presession ID does NOT follow links
    .set_same_site(Some(SameSite::Strict));

    (
        StatusCode::OK,
        Json(PresessionResponse {
            presession_id: presession_info.presession_id,
        }),
    )
        .into_response()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LoginRequest {
    presession_id: Uuid,
}

pub async fn login_handler(signed_cookie_jar: SignedCookieJar) -> Response {
    StatusCode::FORBIDDEN.into_response()
}

#[derive(Debug, Clone, sqlx::Type)]
#[sqlx(type_name = "fingerprint")]
pub struct Fingerprint {
    user_agent: String,
    remote_ip: std::net::IpAddr,
}
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserPresessionRecord {
    presession_id: Uuid,
    csrf_token: [u8; 32],
    fingerprint: Fingerprint,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserSessionRecord {
    session_id: Uuid,
    csrf_token: [u8; 32],
    fingerprint: Fingerprint,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    user_id: Uuid,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("database error")]
    Database(sqlx::Error),
}

pub async fn register_presession<'c, E>(
    conn: E,
    fingerprint: Fingerprint,
    lifetime: Duration,
) -> Result<UserPresessionRecord, SessionError>
where
    E: Executor<'c, Database = Postgres> + Copy,
{
    let presession_id = Uuid::new_v4();
    let created_at = Utc::now();
    let expires_at = created_at + lifetime;
    let upr = UserPresessionRecord {
        presession_id,
        csrf_token: rand::random(),
        fingerprint,
        created_at,
        expires_at,
    };
    let qr = sqlx::query!(
        r#"insert into "user_presessions" values ($1, row($2, $3), $4, $5)"#,
        upr.presession_id,
        upr.fingerprint.user_agent,
        IpNetwork::from(upr.fingerprint.remote_ip),
        upr.created_at,
        upr.expires_at
    )
    .execute(conn)
    .await
    .map_err(SessionError::Database)?;
    debug_assert_eq!(qr.rows_affected(), 1);

    Ok(upr)
}

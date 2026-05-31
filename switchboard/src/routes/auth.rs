//! Interactive OAuth login routes.
//!
//! The flow is the standard authorization-code grant:
//!   1. `GET /auth/github/login` builds the provider authorization URL, persists
//!      the CSRF `state`, and redirects the browser to the provider.
//!   2. The provider redirects back to `GET /auth/github/callback` with `code`
//!      and `state`; we confirm the state, exchange the code, fetch the identity,
//!      provision the local user, and issue a session token.
//!   3. `GET /auth/whoami` reports the identity behind a bearer token.
//!
//! See `OAUTH_LOGIN_PLAN.md` §5–§7.

use crate::auth::Subject;
use crate::auth::oauth::OAuthProvider;
use crate::auth::oauth::github::GithubProvider;
use crate::serve::AppState;
use crate::sql;
use axum::Json;
use axum::extract::{Query, State};
use axum::response::Redirect;
use chrono::{Duration, Utc};
use http::StatusCode;
use serde::Deserialize;
use treadmill_rs::api::switchboard::{AuthToken, LoginResponse, WhoAmIResponse};

/// How long a started login flow's CSRF state remains valid before the callback
/// must arrive.
const FLOW_LIFETIME_MINUTES: i64 = 10;

/// Build the configured GitHub provider, or report why it is unavailable.
fn github_provider(state: &AppState) -> Result<GithubProvider, StatusCode> {
    let cfg = state.config().oauth.github.as_ref().ok_or_else(|| {
        tracing::warn!("GitHub login requested but no [oauth.github] is configured");
        StatusCode::NOT_FOUND
    })?;
    GithubProvider::from_config(cfg).map_err(|e| {
        tracing::error!("failed to build GitHub provider: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

/// `GET /auth/github/login`: redirect the browser into GitHub's consent screen.
#[tracing::instrument(skip(state))]
pub async fn github_login(State(state): State<AppState>) -> Result<Redirect, StatusCode> {
    let provider = github_provider(&state)?;
    let (url, csrf) = provider.authorize().map_err(|e| {
        tracing::error!("failed to build authorization URL: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let expires_at = Utc::now() + Duration::minutes(FLOW_LIFETIME_MINUTES);
    sql::oauth_flow::insert_flow(state.pool(), csrf.secret(), provider.name(), expires_at)
        .await
        .map_err(|e| {
            tracing::error!("failed to persist OAuth flow: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Redirect::to(&url))
}

/// Query parameters GitHub appends to the callback redirect.
#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    code: String,
    state: String,
}

/// `GET /auth/github/callback`: complete the flow and issue a session token.
#[tracing::instrument(skip(state, query))]
pub async fn github_callback(
    State(state): State<AppState>,
    Query(query): Query<CallbackQuery>,
) -> Result<Json<LoginResponse>, StatusCode> {
    let provider = github_provider(&state)?;

    // Confirm the state corresponds to a flow this server started (and matches
    // the provider it was started for) before spending a token exchange on it.
    let flow_provider = sql::oauth_flow::consume_flow(state.pool(), &query.state)
        .await
        .map_err(|e| {
            tracing::error!("failed to consume OAuth flow: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    match flow_provider {
        Some(p) if p == provider.name() => {}
        Some(p) => {
            tracing::warn!("callback state belongs to provider {p}, not {}", provider.name());
            return Err(StatusCode::BAD_REQUEST);
        }
        None => {
            tracing::warn!("callback presented an unknown or expired state");
            return Err(StatusCode::BAD_REQUEST);
        }
    }

    let token = provider.exchange(query.code).await.map_err(|e| {
        tracing::error!("authorization-code exchange failed: {e}");
        StatusCode::BAD_REQUEST
    })?;
    let identity = provider.fetch_identity(&token).await.map_err(|e| {
        tracing::error!("failed to fetch identity: {e}");
        StatusCode::BAD_GATEWAY
    })?;
    // Best-effort: org membership only narrows auto-groups; a failure here must
    // not block an otherwise valid login.
    let org_ids = provider.fetch_org_ids(&token).await.unwrap_or_default();

    // Provision the user and mint the session token atomically: a crash between
    // the two must never leave a user without, or a token referencing a missing
    // user.
    let mut tx = state.pool().begin().await.map_err(|e| {
        tracing::error!("failed to open transaction: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let user_id = sql::user::provision_user(&mut tx, provider.name(), &identity, &org_ids)
        .await
        .map_err(|e| {
            tracing::error!("failed to provision user: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let (session_token, expires_at) = sql::api_token::insert_token(
        &mut *tx,
        user_id,
        state.config().service.default_token_timeout,
    )
    .await
    .map_err(|e| {
        tracing::error!("failed to issue session token: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    tx.commit().await.map_err(|e| {
        tracing::error!("failed to commit login transaction: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    tracing::info!("user {user_id} logged in via {}", provider.name());
    Ok(Json(LoginResponse {
        token: AuthToken::from(session_token),
        expires_at,
    }))
}

/// `GET /auth/whoami`: report the identity behind the presented bearer token.
#[tracing::instrument(skip(state, subject))]
pub async fn whoami(
    State(state): State<AppState>,
    subject: Subject,
) -> Result<Json<WhoAmIResponse>, StatusCode> {
    let user_id = subject.user_id();
    let row = sqlx::query!(
        "select username, full_name from tml_switchboard.users where subject_id = $1;",
        user_id,
    )
    .fetch_one(state.pool())
    .await
    .map_err(|e| {
        tracing::error!("failed to look up user {user_id}: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(WhoAmIResponse {
        user_id,
        username: row.username,
        full_name: row.full_name,
    }))
}

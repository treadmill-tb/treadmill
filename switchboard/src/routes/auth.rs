//! Interactive OAuth login routes.
//!
//! The routes are provider-agnostic: the `{provider}` path segment selects one
//! of the configured [`OAuthProvider`]s via [`provider_for`]. The flow is the
//! standard authorization-code grant:
//!   1. `GET /auth/{provider}/login` builds the provider authorization URL,
//!      persists the CSRF `state`, and redirects the browser to the provider.
//!   2. The provider redirects back to `GET /auth/{provider}/callback` with
//!      `code` and `state`; we confirm the state, exchange the code, fetch the
//!      identity, provision the local user, and issue a session token.
//!   3. `GET /auth/whoami` reports the identity behind a bearer token.
//!

use crate::audit::model::Subject as AuditSubject;
use crate::audit::{self, events};
use crate::auth::Subject;
use crate::auth::oauth::OAuthProvider;
use crate::auth::oauth::github::GithubProvider;
use crate::auth::oauth::mock::{MOCK_IDENTITIES, MockProvider};
use crate::client_addr::ClientAddr;
use crate::http_error::OrInternal;
use crate::serve::AppState;
use crate::sql;
use crate::sql::api_token::IssueSessionToken;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{Duration, Utc};
use http::StatusCode;
use http::request::Parts;
use serde::Deserialize;
use std::collections::HashMap;
use treadmill_rs::api::switchboard::{
    AuthProvidersResponse, AuthToken, LoginResponse, MockIdentityInfo, OAuthProviderInfo,
    WhoAmIResponse,
};

/// How long a started login flow's CSRF state remains valid before the callback
/// must arrive.
const FLOW_LIFETIME_MINUTES: i64 = 10;

/// Resolve the configured [`OAuthProvider`] named by the `{provider}` path
/// segment, or report why it is unavailable (`404` if not configured/enabled).
fn provider_for(
    state: &AppState,
    name: &str,
) -> Result<Box<dyn OAuthProvider + Send + Sync>, StatusCode> {
    match name {
        "github" => {
            let cfg = state.config().oauth.github.as_ref().ok_or_else(|| {
                tracing::warn!("GitHub login requested but no [oauth.github] is configured");
                StatusCode::NOT_FOUND
            })?;
            let provider =
                GithubProvider::from_config(cfg).or_internal("building the GitHub provider")?;
            Ok(Box::new(provider))
        }
        "mock" => {
            let enabled = state
                .config()
                .oauth
                .mock
                .as_ref()
                .map(|m| m.enabled)
                .unwrap_or(false);
            if !enabled {
                tracing::warn!("mock login requested but [oauth.mock] is not enabled");
                return Err(StatusCode::NOT_FOUND);
            }
            Ok(Box::new(MockProvider))
        }
        other => {
            tracing::warn!("login requested for unknown provider {other:?}");
            Err(StatusCode::NOT_FOUND)
        }
    }
}

/// `GET /auth/providers`: advertise the enabled login methods so a frontend can
/// render the right buttons. Unauthenticated; returns only non-secret metadata.
#[tracing::instrument(skip(state))]
pub async fn providers(State(state): State<AppState>) -> Json<AuthProvidersResponse> {
    let oauth_cfg = &state.config().oauth;

    let mut oauth = Vec::new();
    if oauth_cfg.github.is_some() {
        oauth.push(OAuthProviderInfo {
            name: "github".to_string(),
            display_name: "GitHub".to_string(),
            login_path: "/api/v1/auth/github/login".to_string(),
        });
    }

    let mut mock_identities = Vec::new();
    let mock_enabled = oauth_cfg.mock.as_ref().map(|m| m.enabled).unwrap_or(false);
    if mock_enabled {
        for id in MOCK_IDENTITIES {
            let label = if id.admin {
                format!("{} (admin)", id.login)
            } else {
                id.login.to_string()
            };
            mock_identities.push(MockIdentityInfo {
                key: id.key.to_string(),
                label,
                login_path: format!("/api/v1/auth/mock/login?identity={}", id.key),
            });
        }
    }

    Json(AuthProvidersResponse {
        oauth,
        mock_identities,
    })
}

/// `GET /auth/{provider}/login`: start the flow and redirect the browser to the
/// provider's authorization endpoint.
#[tracing::instrument(skip(state, query))]
pub async fn login(
    State(state): State<AppState>,
    Path(provider_name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Redirect, StatusCode> {
    let provider = provider_for(&state, &provider_name)?;
    let (url, csrf) = provider
        .authorize(&query)
        .or_internal("building the authorization URL")?;

    let expires_at = Utc::now() + Duration::minutes(FLOW_LIFETIME_MINUTES);
    sql::oauth_flow::insert_flow(state.pool(), csrf.secret(), provider.name(), expires_at)
        .await
        .or_internal("persisting the OAuth flow")?;

    Ok(Redirect::to(&url))
}

/// Query parameters the provider appends to the callback redirect.
#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    code: String,
    state: String,
}

/// `GET /auth/{provider}/callback`: complete the flow and issue a session token.
#[tracing::instrument(skip(state, query))]
pub async fn callback(
    State(state): State<AppState>,
    Path(provider_name): Path<String>,
    parts: Parts,
    Query(query): Query<CallbackQuery>,
) -> Result<Response, StatusCode> {
    let provider = provider_for(&state, &provider_name)?;

    // Resolve the originating client address and user agent up front, before the
    // request parts are consumed, so they can be stamped onto the issued token
    // and the login audit events.
    let client = ClientAddr::resolve(&parts, &state.config().server);
    let client_ip = client.as_ref().map(ClientAddr::ip_string);
    let client_port = client.and_then(|c| c.port).map(|p| p as i32);
    let user_agent = parts
        .headers
        .get(http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    // Confirm the state corresponds to a flow this server started (and matches
    // the provider it was started for) before spending a token exchange on it.
    let flow_provider = sql::oauth_flow::consume_flow(state.pool(), &query.state)
        .await
        .or_internal("consuming the OAuth flow")?;
    match flow_provider {
        Some(p) if p == provider.name() => {}
        Some(p) => {
            tracing::warn!(
                "callback state belongs to provider {p}, not {}",
                provider.name()
            );
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

    // The mock provider is an unauthenticated bypass; warn loudly on every login
    // it issues so it cannot run unnoticed (it must never be enabled in prod).
    if provider.name() == "mock" {
        tracing::warn!(
            "MOCK LOGIN: issuing a session for unauthenticated mock identity {:?} \
             -- [oauth.mock] must NEVER be enabled in production",
            identity.login,
        );
    }

    // Best-effort: org membership only narrows auto-groups; a failure here must
    // not block an otherwise valid login.
    let org_ids = provider.fetch_org_ids(&token).await.unwrap_or_default();

    // Provision the user and mint the session token atomically: a crash between
    // the two must never leave a user without, or a token referencing a missing
    // user.
    let mut tx = state
        .pool()
        .begin()
        .await
        .or_internal("opening a transaction")?;
    let (user_id, new_user) =
        sql::user::provision_user(&mut tx, provider.name(), &identity, &org_ids)
            .await
            .or_internal("provisioning the user")?;
    // A locked account is still provisioned/refreshed (we keep its data and the
    // provisioning audit trail current), but is refused a token. Commit the
    // refusal — including its audit row — and stop.
    let locked = sqlx::query_scalar!(
        "select locked from tml_switchboard.users where subject_id = $1;",
        user_id,
    )
    .fetch_one(&mut *tx)
    .await
    .or_internal(&format!("reading lock status for {user_id}"))?;
    if locked {
        audit::emit(
            &mut tx,
            &events::LoginDeniedLocked {
                actor: AuditSubject(user_id),
                user: AuditSubject(user_id),
                provider: provider.name().to_string(),
                provider_user_id: identity.provider_user_id.clone(),
                login: identity.login.clone(),
                client_ip: client_ip.clone(),
                client_port,
            },
        )
        .await
        .or_internal("recording the locked-login denial")?;
        tx.commit()
            .await
            .or_internal("committing the locked-login transaction")?;
        tracing::warn!("user {user_id} login denied: account is locked");
        return Err(StatusCode::FORBIDDEN);
    }

    // The mock provider may designate an identity as a global admin (alice). Real
    // providers never do; `grants_global_admin` returns false for them.
    if provider.grants_global_admin(&identity) {
        sql::user::ensure_global_admin(&mut tx, user_id)
            .await
            .or_internal(&format!("granting global admin to {user_id}"))?;
    }

    let (session_token, expires_at) = audit::transition(
        &mut tx,
        IssueSessionToken {
            user_id,
            lifetime: state.config().service.default_token_timeout,
            user_agent: user_agent.clone(),
            comment: Some("interactive OAuth login".to_string()),
            created_ip: client_ip.clone(),
            created_port: client_port,
        },
    )
    .await
    .or_internal("issuing the session token")?;

    // Login marker: one row per successful callback, recording the resolved
    // address and whether this login created the account.
    audit::emit(
        &mut tx,
        &events::UserLoggedIn {
            actor: AuditSubject(user_id),
            user: AuditSubject(user_id),
            provider: provider.name().to_string(),
            provider_user_id: identity.provider_user_id.clone(),
            login: identity.login.clone(),
            new_user,
            client_ip: client_ip.clone(),
            client_port,
        },
    )
    .await
    .or_internal("recording the login event")?;

    tx.commit()
        .await
        .or_internal("committing the login transaction")?;

    tracing::info!("user {user_id} logged in via {}", provider.name());
    let login = LoginResponse {
        token: AuthToken::from(session_token),
        expires_at,
    };

    // Programmatic clients get the token as JSON. A browser frontend cannot
    // consume that without JS, so when a browser-success redirect is configured
    // we hand the token back via a 302 to the frontend instead, which stores it
    // and strips it from the URL. See the config docs and TODOS.md (the token
    // currently transits a URL query string — to be hardened to a back-channel
    // exchange).
    let browser_redirect = state.config().oauth.browser_success_redirect.as_deref();

    match browser_redirect {
        Some(target) => {
            let mut url = url::Url::parse(target).or_internal(&format!(
                "parsing oauth.browser_success_redirect {target:?}"
            ))?;
            url.query_pairs_mut()
                .append_pair("token", &login.token.encode_for_http())
                .append_pair("expires_at", &login.expires_at.to_rfc3339());
            Ok(Redirect::to(url.as_str()).into_response())
        }
        None => Ok(Json(login).into_response()),
    }
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
    .or_internal(&format!("looking up user {user_id}"))?;

    Ok(Json(WhoAmIResponse {
        user_id,
        username: row.username,
        full_name: row.full_name,
    }))
}

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
use crate::auth::admission::{Admission, AdmissionPolicy, DbAdmissionPolicy, DenyReason};
use crate::auth::engine::ANONYMOUS_SUBJECT_ID;
use crate::auth::oauth::ExternalIdentity;
use crate::auth::oauth::OAuthProvider;
use crate::auth::oauth::github::GithubProvider;
use crate::auth::oauth::mock::{MOCK_IDENTITIES, MockProvider};
use crate::client_addr::ClientAddr;
use crate::http_error::OrInternal;
use crate::serve::AppState;
use crate::sql;
use crate::sql::api_token::IssueSessionToken;
use axum::Json;
use axum::extract::{Form, FromRequest, Path, Query, Request, State};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{Duration, Utc};
use http::StatusCode;
use http::request::Parts;
use http::{HeaderValue, header};
use serde::Deserialize;
use std::collections::HashMap;
use treadmill_rs::api::switchboard::{
    AuthProvidersResponse, AuthToken, LoginResponse, MockIdentityInfo, OAuthProviderInfo,
    TosAcceptRequest, TosInfoResponse, TosRequiredResponse, WhoAmIResponse,
};
use uuid::Uuid;

/// How long a started login flow's CSRF state remains valid before the callback
/// must arrive.
const FLOW_LIFETIME_MINUTES: i64 = 10;

/// How long a staged, ToS-awaiting registration (`pending_registrations`) lives
/// before it must be consumed by `POST /auth/tos/accept`.
const TOS_LIFETIME_MINUTES: i64 = 30;

/// Name of the `HttpOnly` cookie carrying the pending-registration id across the
/// ToS interstitial, so a browser can `POST /auth/tos/accept` without echoing
/// the id itself.
const PENDING_COOKIE: &str = "tml_pending_registration";

/// The blanket Terms of Service text served by `GET /auth/tos`. A placeholder
/// until a real ToS exists; its version is [`crate::config::ServiceConfig::current_tos_version`].
pub const TOS_TEXT: &str = "By using this Treadmill instance you agree to its terms of service. \
     (Placeholder — no formal ToS is in effect yet.)";

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

    // Resolve the existing local user, if any, WITHOUT writing. The admission
    // gate is consulted only when this is a brand-new subject (a `None`
    // resolution); existing users -- including a new identity linked to an
    // existing account by a shared verified email -- are never gated.
    let mut conn = state
        .pool()
        .acquire()
        .await
        .or_internal("acquiring a database connection")?;
    let resolved = sql::user::resolve_user(&mut conn, provider.name(), &identity)
        .await
        .or_internal("resolving the user")?;
    drop(conn);

    // The Terms of Service version currently in force; a login cannot complete
    // until the user has accepted at least this version.
    let current_tos = state.config().service.current_tos_version;

    // Branches that log in immediately converge on `(tx, user_id, new_user)` and
    // run the shared locked-check / admin-grant / token / login-marker tail
    // below. Branches that still need ToS consent stage a `pending_registrations`
    // row and return the interstitial (a 302 to the ToS page, or a 409 marker)
    // WITHOUT issuing a token -- see `stage_and_interstitial`.
    let (mut tx, user_id, new_user) = match resolved {
        Some((user_id, kind)) => {
            // Existing user: proceed ungated. Org membership only narrows
            // auto-groups, so a fetch failure here must not block the login.
            let org_ids = provider.fetch_org_ids(&token).await.unwrap_or_default();
            let mut tx = state
                .pool()
                .begin()
                .await
                .or_internal("opening a transaction")?;
            sql::user::provision_existing_user(
                &mut tx,
                provider.name(),
                &identity,
                &org_ids,
                user_id,
                kind,
            )
            .await
            .or_internal("refreshing the existing user")?;

            // A ToS version bump forces re-acceptance before a token is issued,
            // but only for an unlocked account -- a locked account is handled by
            // the shared locked-login denial in the tail instead. Read both here.
            let status = sqlx::query!(
                "select locked, tos_accepted_version \
                 from tml_switchboard.users where subject_id = $1;",
                user_id,
            )
            .fetch_one(&mut *tx)
            .await
            .or_internal(&format!("reading login state for {user_id}"))?;
            let stale_tos = status.tos_accepted_version.is_none_or(|v| v < current_tos);
            if !status.locked && stale_tos {
                // Commit the allowed profile/email/group refresh, then stage a
                // re-acceptance and bounce the user through the interstitial.
                tx.commit()
                    .await
                    .or_internal("committing the profile refresh")?;
                return stage_and_interstitial(
                    &state,
                    provider.name(),
                    None,
                    Some(user_id),
                    &org_ids,
                    current_tos,
                )
                .await;
            }

            (tx, user_id, false)
        }
        None => {
            // Brand-new subject. The development-only mock provider is an
            // unauthenticated bypass whose whole purpose is to conjure users, so
            // it is never subject to the admission gate; every real provider is.
            let gated = provider.name() != "mock";

            // Org membership is load-bearing for org-based admission at
            // registration, so a fetch failure on the gated new-user path fails
            // closed (a retryable deny) rather than silently admitting nobody.
            let org_ids = if gated {
                match provider.fetch_org_ids(&token).await {
                    Ok(ids) => ids,
                    Err(e) => {
                        tracing::warn!(
                            "org lookup failed during {} registration for {}: {e}",
                            provider.name(),
                            identity.login,
                        );
                        record_registration_denied(
                            &state,
                            provider.name(),
                            &identity,
                            DenyReason::OrgLookupFailed,
                            client_ip.clone(),
                            client_port,
                        )
                        .await?;
                        return Err(StatusCode::FORBIDDEN);
                    }
                }
            } else {
                provider.fetch_org_ids(&token).await.unwrap_or_default()
            };

            if gated {
                let policy = DbAdmissionPolicy::new(state.pool().clone());
                let verdict = policy
                    .admit(provider.name(), &identity, &org_ids)
                    .await
                    .or_internal("consulting the admission gate")?;
                if let Admission::Deny(reason) = verdict {
                    record_registration_denied(
                        &state,
                        provider.name(),
                        &identity,
                        reason,
                        client_ip.clone(),
                        client_port,
                    )
                    .await?;
                    tracing::warn!(
                        "registration denied for {} via {}: {}",
                        identity.login,
                        provider.name(),
                        reason.as_str(),
                    );
                    return Err(StatusCode::FORBIDDEN);
                }

                // Admitted, but no durable user record may exist before the user
                // accepts the ToS. Stage the identity + org ids and return the
                // interstitial; the account is created only in `tos_accept`.
                return stage_and_interstitial(
                    &state,
                    provider.name(),
                    Some(&identity),
                    None,
                    &org_ids,
                    current_tos,
                )
                .await;
            }

            // Mock bypass: the dev-only provider is exempt from both the gate and
            // the ToS interstitial, so it conjures the account immediately, with
            // the ToS recorded as already accepted at the current version.
            let mut tx = state
                .pool()
                .begin()
                .await
                .or_internal("opening a transaction")?;
            let user_id = sql::user::create_and_reconcile(
                &mut tx,
                provider.name(),
                &identity,
                &org_ids,
                current_tos,
            )
            .await
            .or_internal("provisioning the user")?;
            (tx, user_id, true)
        }
    };

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
    login_success_response(&state, login)
}

/// Turn a successful login into its HTTP response. Programmatic clients get the
/// token as JSON; when a browser-success redirect is configured we hand the
/// token back via a 302 to the frontend instead, which stores it and strips it
/// from the URL. See the config docs and TODOS.md (the token currently transits
/// a URL query string — to be hardened to a back-channel exchange). Shared by
/// the OAuth callback and the ToS-accept handler so the two cannot diverge.
fn login_success_response(state: &AppState, login: LoginResponse) -> Result<Response, StatusCode> {
    match state.config().oauth.browser_success_redirect.as_deref() {
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

/// Stage a `pending_registrations` row for a login awaiting ToS acceptance and
/// return the interstitial response. Provide EITHER `identity` (a brand-new
/// admitted user) OR `existing_user_id` (a re-acceptance), never both. No token
/// is issued and, for a new user, no durable record exists yet — the account is
/// created only when `POST /auth/tos/accept` consumes this row.
async fn stage_and_interstitial(
    state: &AppState,
    provider: &str,
    identity: Option<&ExternalIdentity>,
    existing_user_id: Option<Uuid>,
    org_ids: &[String],
    tos_version: i32,
) -> Result<Response, StatusCode> {
    let pending_id = Uuid::new_v4();
    let expires_at = Utc::now() + Duration::minutes(TOS_LIFETIME_MINUTES);
    sql::pending_registration::insert_pending(
        state.pool(),
        pending_id,
        provider,
        identity,
        existing_user_id,
        org_ids,
        expires_at,
    )
    .await
    .or_internal("staging the pending registration")?;
    tos_interstitial(state, pending_id, tos_version)
}

/// Build the ToS interstitial response: a 302 to the configured browser ToS page
/// when `oauth.browser_tos_redirect` is set (a browser frontend), else a `409`
/// `tos_required` JSON marker (a programmatic client). Both carry the pending id
/// in an `HttpOnly` cookie so the browser can `POST /auth/tos/accept` blind; the
/// redirect additionally carries `?pending_id=…&tos_version=…` so a frontend on
/// another origin (where that cookie never arrives) can embed the id in its
/// accept form instead. The id is single-use and short-lived, so its transit
/// through the URL is far less sensitive than the token handoff documented in
/// TODOS.md.
fn tos_interstitial(
    state: &AppState,
    pending_id: Uuid,
    tos_version: i32,
) -> Result<Response, StatusCode> {
    let cookie = format!(
        "{PENDING_COOKIE}={pending_id}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
        TOS_LIFETIME_MINUTES * 60,
    );
    let cookie = HeaderValue::from_str(&cookie).or_internal("building the pending cookie")?;
    let tos_url = state.config().oauth.browser_tos_redirect.clone();

    let mut response = match &tos_url {
        Some(target) => {
            let mut url = url::Url::parse(target)
                .or_internal(&format!("parsing oauth.browser_tos_redirect {target:?}"))?;
            url.query_pairs_mut()
                .append_pair("pending_id", &pending_id.to_string())
                .append_pair("tos_version", &tos_version.to_string());
            Redirect::to(url.as_str()).into_response()
        }
        None => (
            StatusCode::CONFLICT,
            Json(TosRequiredResponse {
                tos_required: true,
                pending_id,
                tos_version,
                tos_url: tos_url.clone(),
            }),
        )
            .into_response(),
    };
    response.headers_mut().insert(header::SET_COOKIE, cookie);
    Ok(response)
}

/// `GET /auth/tos`: the current Terms of Service text + version for a frontend to
/// render on the interstitial. Unauthenticated.
#[tracing::instrument(skip(state))]
pub async fn tos_info(State(state): State<AppState>) -> Json<TosInfoResponse> {
    Json(TosInfoResponse {
        version: state.config().service.current_tos_version,
        text: TOS_TEXT.to_string(),
    })
}

/// The request body of `POST /auth/tos/accept`, in whichever encoding the
/// client speaks: JSON for programmatic clients, `x-www-form-urlencoded` for
/// the console's no-JS HTML form (forms cannot send JSON). Any other or absent
/// body extracts as an empty request (the cookie then identifies the staged
/// registration).
pub struct TosAcceptBody(TosAcceptRequest);

impl FromRequest<AppState> for TosAcceptBody {
    type Rejection = StatusCode;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, StatusCode> {
        let content_type = req
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if content_type.starts_with("application/json") {
            let Json(body) = Json::<TosAcceptRequest>::from_request(req, state)
                .await
                .map_err(|_| StatusCode::BAD_REQUEST)?;
            Ok(Self(body))
        } else if content_type.starts_with("application/x-www-form-urlencoded") {
            let Form(body) = Form::<TosAcceptRequest>::from_request(req, state)
                .await
                .map_err(|_| StatusCode::BAD_REQUEST)?;
            Ok(Self(body))
        } else {
            Ok(Self(TosAcceptRequest::default()))
        }
    }
}

/// `POST /auth/tos/accept`: finish a login staged behind the ToS interstitial.
///
/// Unauthenticated; the staged registration is identified by the pending id,
/// taken from the body (JSON or form-encoded, see [`TosAcceptBody`]) or the
/// `HttpOnly` cookie the callback set. In one transaction: consume the staging
/// row (missing/expired → `410 Gone`), then either create the brand-new account
/// (recording the accepted ToS version) or record an existing user's
/// re-acceptance, and issue the session token. Returns the same success shape
/// as the callback.
#[tracing::instrument(skip(state, body))]
pub async fn tos_accept(
    State(state): State<AppState>,
    parts: Parts,
    body: TosAcceptBody,
) -> Result<Response, StatusCode> {
    // The pending id may arrive in the body (programmatic, or the console's
    // form) or the cookie (browser). Prefer the body, fall back to the cookie.
    let pending_id = body
        .0
        .pending_id
        .or_else(|| cookie_value(&parts, PENDING_COOKIE).and_then(|v| Uuid::parse_str(v).ok()))
        .ok_or(StatusCode::BAD_REQUEST)?;

    let client = ClientAddr::resolve(&parts, &state.config().server);
    let client_ip = client.as_ref().map(ClientAddr::ip_string);
    let client_port = client.and_then(|c| c.port).map(|p| p as i32);
    let user_agent = parts
        .headers
        .get(http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let current_tos = state.config().service.current_tos_version;

    let mut tx = state
        .pool()
        .begin()
        .await
        .or_internal("opening the ToS-accept transaction")?;

    // Consume-once: an unknown/expired/already-used id is a 410.
    let Some(pending) = sql::pending_registration::consume_pending(&mut *tx, pending_id)
        .await
        .or_internal("consuming the pending registration")?
    else {
        return Err(StatusCode::GONE);
    };

    let user_id = if let Some(user_id) = pending.existing_user_id {
        // Existing user re-accepting a bumped ToS: record acceptance, no new
        // record, and (deliberately) no locked-check here — the callback already
        // routed a locked account down the locked-login-denial path.
        sqlx::query!(
            "update tml_switchboard.users \
             set tos_accepted_version = $2, tos_accepted_at = now() \
             where subject_id = $1;",
            user_id,
            current_tos,
        )
        .execute(&mut *tx)
        .await
        .or_internal("recording ToS re-acceptance")?;
        audit::emit(
            &mut tx,
            &events::TosAccepted {
                actor: AuditSubject(user_id),
                user: AuditSubject(user_id),
                version: current_tos,
            },
        )
        .await
        .or_internal("recording the ToS-acceptance event")?;
        user_id
    } else {
        // Brand-new admitted user: create the account now, at the accepted
        // version, then emit the login marker the callback's Admit path used to.
        let identity = pending
            .parse_identity()
            .ok_or(StatusCode::GONE)?
            .or_internal("decoding the staged identity")?;
        let user_id = sql::user::create_and_reconcile(
            &mut tx,
            &pending.provider,
            &identity,
            &pending.org_ids,
            current_tos,
        )
        .await
        .or_internal("provisioning the user")?;
        audit::emit(
            &mut tx,
            &events::UserLoggedIn {
                actor: AuditSubject(user_id),
                user: AuditSubject(user_id),
                provider: pending.provider.clone(),
                provider_user_id: identity.provider_user_id.clone(),
                login: identity.login.clone(),
                new_user: true,
                client_ip: client_ip.clone(),
                client_port,
            },
        )
        .await
        .or_internal("recording the login event")?;
        user_id
    };

    let (session_token, expires_at) = audit::transition(
        &mut tx,
        IssueSessionToken {
            user_id,
            lifetime: state.config().service.default_token_timeout,
            user_agent: user_agent.clone(),
            comment: Some("interactive OAuth login (ToS accepted)".to_string()),
            created_ip: client_ip.clone(),
            created_port: client_port,
        },
    )
    .await
    .or_internal("issuing the session token")?;

    tx.commit()
        .await
        .or_internal("committing the ToS-accept transaction")?;

    tracing::info!("user {user_id} accepted ToS v{current_tos} and logged in");
    login_success_response(
        &state,
        LoginResponse {
            token: AuthToken::from(session_token),
            expires_at,
        },
    )
}

/// Read a named cookie's value out of the request's `Cookie` header, if present.
fn cookie_value<'a>(parts: &'a Parts, name: &str) -> Option<&'a str> {
    parts
        .headers
        .get(http::header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| k.trim() == name)
        .map(|(_, v)| v.trim())
}

/// Record an admission-gate denial as an operator-only audit event and commit
/// it, leaving NO user record. Attributed to the well-known anonymous subject
/// (the denied party has no local subject of their own). The caller returns
/// `403 Forbidden` after this succeeds.
async fn record_registration_denied(
    state: &AppState,
    provider: &str,
    identity: &ExternalIdentity,
    reason: DenyReason,
    client_ip: Option<String>,
    client_port: Option<i32>,
) -> Result<(), StatusCode> {
    let mut tx = state
        .pool()
        .begin()
        .await
        .or_internal("opening the denial transaction")?;
    audit::emit(
        &mut tx,
        &events::RegistrationDenied {
            actor: AuditSubject(ANONYMOUS_SUBJECT_ID),
            provider: provider.to_string(),
            provider_user_id: identity.provider_user_id.clone(),
            login: identity.login.clone(),
            reason: reason.as_str().to_string(),
            client_ip,
            client_port,
        },
    )
    .await
    .or_internal("recording the registration denial")?;
    tx.commit()
        .await
        .or_internal("committing the denial transaction")?;
    Ok(())
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

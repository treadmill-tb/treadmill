//! Interactive OAuth login routes.
//!
//! The routes are provider-agnostic: the `{provider}` path segment selects one
//! of the configured [`OAuthProvider`]s via [`provider_for`]. The flow is the
//! standard authorization-code grant, with every login staged server-side and
//! claimed at the completion endpoint — the only place a session token is
//! minted:
//!
//!   1. `GET /auth/{provider}/login` builds the provider authorization URL,
//!      persists the CSRF `state` — plus a browser frontend's optional,
//!      allowlist-validated `return_to` — and redirects the browser to the
//!      provider.
//!
//!   2. The provider redirects back to `GET /auth/{provider}/callback` with
//!      `code` and `state`; we confirm the state, exchange the code, fetch the
//!      identity, provision/refresh the local user, and stage the login. A
//!      flow that declared a `return_to` sends the browser there with the
//!      single-use staged pair in the query; other flows receive the pair as
//!      JSON. No token is issued here.
//!
//!   3. `POST /auth/login/complete` consumes the staged pair and mints the
//!      session token, once everything the staging marked `required` (e.g.,
//!      ToS consent) is provided.
//!
//!   4. `GET /auth/whoami` reports the identity behind a bearer token.

use crate::audit::model::Subject as AuditSubject;
use crate::audit::{self, events};
use crate::auth::Subject;
use crate::auth::admission::{Admission, AdmissionPolicy, DbAdmissionPolicy, DenyReason};
use crate::auth::engine::ANONYMOUS_SUBJECT_ID;
use crate::auth::oauth::ExternalIdentity;
use crate::auth::oauth::OAuthProvider;
use crate::auth::oauth::github::GithubProvider;
use crate::auth::oauth::mock::{MOCK_IDENTITIES, MockProvider};
use crate::auth::staged_secret;
use crate::client_addr::ClientAddr;
use crate::config::ServerConfig;
use crate::http_error::OrInternal;
use crate::routes::params::ProviderPath;
use crate::serve::AppState;
use crate::sql;
use crate::sql::api_token::IssueSessionToken;
use crate::sql::staged_login::{StageLogin, StagedLogin};
use axum::Json;
use axum::extract::{Form, FromRequest, Path, Query, Request, State};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{Duration, Utc};
use http::StatusCode;
use http::request::Parts;
use indoc::indoc;
use serde::Deserialize;
use sqlx::PgExecutor;
use std::collections::HashMap;
use treadmill_rs::api::switchboard::{
    AuthProvidersResponse, AuthToken, LoginCompleteRequest, LoginResponse, LoginStagedResponse,
    MockIdentityInfo, OAuthProviderInfo, TosInfoResponse, WhoAmIResponse,
};
use uuid::Uuid;

/// How long a started login flow's CSRF state remains valid before the callback
/// must arrive.
const FLOW_LIFETIME_MINUTES: i64 = 10;

/// How long a staged login lives before it must be consumed by `POST
/// /auth/login/complete`, when its pair is held by a human working through the
/// completion step (reading the ToS).
const STAGED_LOGIN_LIFETIME_MINUTES: i64 = 30;

/// How long a staged login lives when its pair merely transits a browser
/// redirect to the flow's `return_to`: the frontend exchanges it within
/// seconds, and anything longer only widens the window in which an abandoned
/// redirect (whose pair sits in browser history) stays claimable.
const STAGED_HANDOFF_LIFETIME_MINUTES: i64 = 2;

/// The blanket Terms of Service text served by `GET /auth/tos`. A placeholder
/// until a real ToS exists; its version is [`crate::config::ServiceConfig::current_tos_version`].
pub const TOS_TEXT: &str = "TODO";

/// Resolve the configured [`OAuthProvider`] named by the `{provider}` path
/// segment, or report why it is unavailable (`404` if not configured/enabled).
fn provider_for(
    state: &AppState,
    name: &str,
) -> Result<Box<dyn OAuthProvider + Send + Sync>, StatusCode> {
    match name {
        "github" => {
            let cfg = state.config().oauth.github.as_ref().ok_or_else(|| {
                tracing::debug!("GitHub login requested but no [oauth.github] is configured");
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
                tracing::debug!("mock login requested but [oauth.mock] is not enabled");
                return Err(StatusCode::NOT_FOUND);
            }
            Ok(Box::new(MockProvider))
        }
        other => {
            tracing::debug!("login requested for unknown provider {other:?}");
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

pub const AUTH_LOGIN_ENDPOINT_DOC: &str = indoc! {"
    Begin an authorization-code flow with the given provider.

    This endpoint emits a 303 \"See Other\" redirect to the authentication
    provider's consent screen. A client may pass `?return_to=<URL>` (validated
    against a server-side allowlist) to have the callback redirect the browser
    following a successful token-exchange with the authenthenication provider.
    On redirect, the `(staged_id, staged_secret)` pair will be placed in request
    parameters of the `return_to` URL; without it the callback responds with
    JSON.
"};

/// `GET /auth/{provider}/login`: start the flow and redirect the browser to the
/// provider's authorization endpoint.
///
/// A browser frontend passes `?return_to=<its landing URL>` to declare where
/// the callback should send the browser afterwards, carrying the single-use
/// staged pair in the query. It must match the configured allowlist exactly
/// (see [`crate::config::OAuthConfig::return_to_allowlist`]) — the staged pair
/// is a token-minting capability — so anything else is rejected up front. A
/// flow without `return_to` (a programmatic client) receives JSON from the
/// callback instead.
#[tracing::instrument(skip(state, query))]
pub async fn login(
    State(state): State<AppState>,
    Path(ProviderPath {
        provider: provider_name,
    }): Path<ProviderPath>,
    Query(mut query): Query<HashMap<String, String>>,
) -> Result<Redirect, StatusCode> {
    let provider = provider_for(&state, &provider_name)?;

    // `return_to` addresses the switchboard, not the provider: pop it before
    // building the provider authorization URL (which interprets the remaining,
    // provider-specific parameters, e.g. the mock provider's `identity`).
    let return_to = query.remove("return_to");
    if let Some(target) = return_to.as_deref()
        && !state.config().return_to_allowed(target)
    {
        tracing::warn!("rejecting login with non-allowlisted return_to {target:?}");
        return Err(StatusCode::BAD_REQUEST);
    }

    let (url, csrf) = provider
        .authorize(&query)
        .or_internal("building the authorization URL")?;

    let expires_at = Utc::now() + Duration::minutes(FLOW_LIFETIME_MINUTES);
    sql::oauth_flow::insert_flow(
        state.pool(),
        csrf.secret(),
        provider.name(),
        return_to.as_deref(),
        expires_at,
    )
    .await
    .or_internal("persisting the OAuth flow")?;

    Ok(Redirect::to(&url))
}

pub const AUTH_PROVIDER_CALLBACK_ENDPOINT_DOC: &str = indoc! {"
    The authentication provider's redirect target; not called directly.

    Performs a token-exchange with the authentication provider, and stages a new
    login. This endpoint does not mint a token directly; instead a client must
    complete the login with an additional request to `/auth/login/complete` by
    supplying the returned `(staged_id, staged_secret)` tuple. This tuple is
    either returned as JSON or, for a flow that declared a `return_to`
    parameter, via a 302 \"See Other\" redirect to an URL with those added as
    query parameters.

    A login may require additional information by the user (such as an explicit
    ToS accept). See the `/auth/login/complete` endpoint docs.
"};

/// Query parameters the provider appends to the callback redirect.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CallbackQuery {
    code: String,
    state: String,
}

/// `GET /auth/{provider}/callback`: complete the flow and stage the login. No
/// session token is issued here — every outcome hands the caller a single-use
/// staged pair (via the flow's `return_to` redirect, or as JSON) that `POST
/// /auth/login/complete` exchanges for the token.
#[tracing::instrument(skip(state, query))]
pub async fn callback(
    State(state): State<AppState>,
    Path(ProviderPath {
        provider: provider_name,
    }): Path<ProviderPath>,
    parts: Parts,
    Query(query): Query<CallbackQuery>,
) -> Result<Response, StatusCode> {
    let provider = provider_for(&state, &provider_name)?;

    // Resolve the logging-in browser's address and user agent up front, before
    // the request parts are consumed. They are stamped onto the staged login —
    // and from there onto the session token minted at completion, which for a
    // browser flow is requested by the frontend's server, not the user.
    let ctx = ClientContext::resolve(&parts, &state.config().server);
    let client_ip = ctx.ip.clone();
    let client_port = ctx.port;

    // Confirm the state corresponds to a flow this server started (and matches
    // the provider it was started for) before spending a token exchange on it.
    let flow = sql::oauth_flow::consume_flow(state.pool(), &query.state)
        .await
        .or_internal("consuming the OAuth flow")?;
    let flow = match flow {
        Some(f) if f.provider == provider.name() => f,
        Some(f) => {
            tracing::warn!(
                "callback state belongs to provider {}, not {}",
                f.provider,
                provider.name()
            );
            return Err(StatusCode::BAD_REQUEST);
        }
        None => {
            tracing::warn!("callback presented an unknown or expired state");
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    // The flow's return_to was validated at initiation; re-check it against the
    // allowlist in case the configuration changed while the user was at the
    // provider — a since-removed entry must not keep receiving staged pairs.
    if let Some(target) = flow.return_to.as_deref()
        && !state.config().return_to_allowed(target)
    {
        tracing::warn!("rejecting callback: flow return_to {target:?} no longer allowlisted");
        return Err(StatusCode::BAD_REQUEST);
    }
    let return_to = flow.return_to.as_deref();

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

    // Every branch converges on staging the (now verified) login: an existing
    // user is refreshed and staged, a brand-new admitted user's identity is
    // staged without any durable record, and the dev-only mock provider
    // conjures its account and stages it ready-to-claim. The staged pair is
    // then handed back by `stage_and_respond`; the session token is only
    // minted when `POST /auth/login/complete` consumes the row.
    let (user_id, org_ids, tos_required) = match resolved {
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

            // A locked account is still refreshed (we keep its data and the
            // provisioning audit trail current), but is refused up front --
            // nothing gets staged. Commit the refusal, including its audit row.
            let status = sqlx::query!(
                "select locked, tos_accepted_version \
                 from tml_switchboard.users where subject_id = $1;",
                user_id,
            )
            .fetch_one(&mut *tx)
            .await
            .or_internal(&format!("reading login state for {user_id}"))?;
            if status.locked {
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

            tx.commit()
                .await
                .or_internal("committing the profile refresh")?;

            // A ToS version bump forces re-acceptance before the login can be
            // claimed.
            let stale_tos = status.tos_accepted_version.is_none_or(|v| v < current_tos);
            (user_id, org_ids, stale_tos)
        }
        None => {
            // Org membership is load-bearing for org-based admission at
            // registration, so a fetch failure on the gated new-user path fails
            // closed (a retryable deny) rather than silently admitting nobody.
            let org_ids = match provider.fetch_org_ids(&token).await {
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
            };

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
            // accepts the ToS. Stage the identity + org ids; the account is
            // created only when `login_complete` consumes the row.
            return stage_and_respond(
                &state,
                provider.name(),
                Some(&identity),
                None,
                &org_ids,
                return_to,
                &ctx,
                true,
                current_tos,
            )
            .await;
        }
    };

    tracing::info!(
        "staged a login for user {user_id} via {} (ToS consent required: {tos_required})",
        provider.name()
    );
    stage_and_respond(
        &state,
        provider.name(),
        None,
        Some(user_id),
        &org_ids,
        return_to,
        &ctx,
        tos_required,
        current_tos,
    )
    .await
}

/// The logging-in browser's resolved address and user agent. Captured at the
/// OAuth callback and stored on the staged login, so the session token minted
/// later at completion is stamped with the user's context — for a browser
/// flow, the completing caller is the frontend's server, not the user.
#[derive(Debug, Clone, Default)]
struct ClientContext {
    ip: Option<String>,
    port: Option<i32>,
    user_agent: Option<String>,
}

impl ClientContext {
    /// Resolve from an incoming request's parts (before they are consumed).
    fn resolve(parts: &Parts, server: &ServerConfig) -> Self {
        let client = ClientAddr::resolve(parts, server);
        ClientContext {
            ip: client.as_ref().map(ClientAddr::ip_string),
            port: client.and_then(|c| c.port).map(|p| p as i32),
            user_agent: parts
                .headers
                .get(http::header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
        }
    }

    /// The context a staged login stored at the callback, for carrying it
    /// forward onto a re-staged/successor row.
    fn of_staged(staged: &StagedLogin) -> Self {
        ClientContext {
            ip: staged.created_ip.clone(),
            port: staged.created_port,
            user_agent: staged.user_agent.clone(),
        }
    }
}

/// Stage a `staged_logins` row and return the `(staged_id, staged_secret)`
/// pair. Provide EITHER `identity` (a brand-new admitted user — no durable
/// record exists until `POST /auth/login/complete` consumes the row) OR
/// `existing_user_id`, never both. The id alone is no capability; only the
/// secret's salted hash is stored. Takes any executor so a re-stage/successor
/// row can join the completion transaction consuming its predecessor.
#[allow(clippy::too_many_arguments)]
async fn stage_login_row(
    conn: impl PgExecutor<'_>,
    provider: &str,
    identity: Option<&ExternalIdentity>,
    existing_user_id: Option<Uuid>,
    org_ids: &[String],
    return_to: Option<&str>,
    ctx: &ClientContext,
    lifetime_minutes: i64,
) -> Result<(Uuid, String), StatusCode> {
    let staged_id = Uuid::new_v4();
    let staged_secret = staged_secret::generate();
    let secret_hash =
        staged_secret::hash(&staged_secret).or_internal("hashing the staged secret")?;
    sql::staged_login::insert_staged(
        conn,
        StageLogin {
            id: staged_id,
            secret_hash: &secret_hash,
            provider,
            identity,
            existing_user_id,
            org_ids,
            return_to,
            user_agent: ctx.user_agent.as_deref(),
            created_ip: ctx.ip.as_deref(),
            created_port: ctx.port,
            expires_at: Utc::now() + Duration::minutes(lifetime_minutes),
        },
    )
    .await
    .or_internal("persisting the staged login")?;
    Ok((staged_id, staged_secret))
}

/// Hand a staged pair to the client: a 302 to `redirect_to` with the
/// single-use pair in the query when the response goes back to a browser (the
/// flow declared a `return_to`, and — for the completion route — the request
/// came from the frontend's HTML form), else the [`LoginStagedResponse`] JSON
/// with `status`.
fn staged_response(
    redirect_to: Option<&str>,
    status: StatusCode,
    staged_id: Uuid,
    staged_secret: String,
    required: Vec<String>,
    tos_version: Option<i32>,
) -> Result<Response, StatusCode> {
    match redirect_to {
        Some(target) => {
            let mut url = url::Url::parse(target)
                .or_internal(&format!("parsing allowlisted return_to {target:?}"))?;
            url.query_pairs_mut()
                .append_pair("staged_id", &staged_id.to_string())
                .append_pair("staged_secret", &staged_secret);
            Ok(Redirect::to(url.as_str()).into_response())
        }
        None => Ok((
            status,
            Json(LoginStagedResponse {
                required,
                staged_id,
                staged_secret,
                tos_version,
            }),
        )
            .into_response()),
    }
}

/// The callback's staging tail: stage the verified login and hand its pair
/// back. The row's lifetime follows how the pair travels — a browser flow's
/// pair only transits the `return_to` redirect (the frontend exchanges it
/// within seconds), while a programmatic client may hold the pair while a
/// human works through the completion step.
#[allow(clippy::too_many_arguments)]
async fn stage_and_respond(
    state: &AppState,
    provider: &str,
    identity: Option<&ExternalIdentity>,
    existing_user_id: Option<Uuid>,
    org_ids: &[String],
    return_to: Option<&str>,
    ctx: &ClientContext,
    tos_required: bool,
    current_tos: i32,
) -> Result<Response, StatusCode> {
    // Housekeeping on the callback path (the completion route re-stages within
    // its own transaction and skips this).
    sql::staged_login::sweep_expired(state.pool())
        .await
        .or_internal("sweeping expired staged logins")?;

    let lifetime = if return_to.is_some() {
        STAGED_HANDOFF_LIFETIME_MINUTES
    } else {
        STAGED_LOGIN_LIFETIME_MINUTES
    };
    let (staged_id, staged_secret) = stage_login_row(
        state.pool(),
        provider,
        identity,
        existing_user_id,
        org_ids,
        return_to,
        ctx,
        lifetime,
    )
    .await?;

    let required = if tos_required {
        vec!["tos".to_string()]
    } else {
        Vec::new()
    };
    staged_response(
        return_to,
        StatusCode::OK,
        staged_id,
        staged_secret,
        required,
        tos_required.then_some(current_tos),
    )
}

/// `GET /auth/tos`: the current Terms of Service text + version for a frontend to
/// render on the login-completion page. Unauthenticated.
#[tracing::instrument(skip(state))]
pub async fn tos_info(State(state): State<AppState>) -> Json<TosInfoResponse> {
    Json(TosInfoResponse {
        version: state.config().service.current_tos_version,
        text: TOS_TEXT.to_string(),
    })
}

pub const AUTH_LOGIN_COMPLETE_ENDPOINT_DOC: &str = indoc! {"
    Claim a staged login by providing the `(staged_id, staged_secret)` tuple
    provided by the callback response or redirect.

    Completing the login may require supplying additional values. If `required`
    includes `\"tos\"`, the `\"tos_version\"` field must be the current ToS
    version.

    Accepts the same fields form-encoded (for no-JS browser frontends).
"};

/// The request body of `POST /auth/login/complete`, in whichever encoding the
/// client speaks: JSON for programmatic clients, `x-www-form-urlencoded` for
/// the console's no-JS HTML form (forms cannot send JSON). The body is
/// mandatory — it carries the staged pair — so any other content type is
/// `415` and a malformed body is `400`.
pub struct LoginCompleteBody {
    request: LoginCompleteRequest,
    /// Whether the body arrived form-encoded — i.e. from a browser's HTML form
    /// (the console's ToS consent form), which cannot consume a JSON response.
    /// The handler answers such requests through the flow's `return_to`
    /// redirect; JSON callers always get JSON, so a frontend's server-to-server
    /// exchange of a browser flow's pair is never redirected.
    from_form: bool,
}

// Document the JSON variant of the body; the form-encoded variant carries the
// same fields and exists only for the console's no-JS HTML form.
impl aide::OperationInput for LoginCompleteBody {
    fn operation_input(
        ctx: &mut aide::generate::GenContext,
        operation: &mut aide::openapi::Operation,
    ) {
        Json::<LoginCompleteRequest>::operation_input(ctx, operation);
    }
}

impl FromRequest<AppState> for LoginCompleteBody {
    type Rejection = StatusCode;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, StatusCode> {
        let content_type = req
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if content_type.starts_with("application/json") {
            let Json(body) = Json::<LoginCompleteRequest>::from_request(req, state)
                .await
                .map_err(|_| StatusCode::BAD_REQUEST)?;
            Ok(Self {
                request: body,
                from_form: false,
            })
        } else if content_type.starts_with("application/x-www-form-urlencoded") {
            let Form(body) = Form::<LoginCompleteRequest>::from_request(req, state)
                .await
                .map_err(|_| StatusCode::BAD_REQUEST)?;
            Ok(Self {
                request: body,
                from_form: true,
            })
        } else {
            Err(StatusCode::UNSUPPORTED_MEDIA_TYPE)
        }
    }
}

/// `POST /auth/login/complete`: claim a staged login — the sole point a
/// session token is minted.
///
/// Unauthenticated; the caller authenticates by presenting the staged login's
/// `staged_id` TOGETHER with its one-time `staged_secret` (JSON or
/// form-encoded, see [`LoginCompleteBody`]). In one transaction: consume the
/// staging row (unknown id, wrong secret, or expired → `410 Gone`, with a
/// wrong secret leaving the row intact), re-check the lock state for an
/// existing user, and check what the login still requires. If ToS consent is
/// required, the echoed `tos_version` must be the one currently in force — a
/// concurrent ToS bump must not record consent to text the user never saw —
/// else the presented pair is consumed and a fresh one is returned as a `409`
/// marker (or through the flow's `return_to` for a browser form) for the
/// client to re-render and re-submit. A satisfied completion creates the
/// brand-new account (recording the accepted ToS version) or records the
/// re-acceptance, then either mints the token (JSON callers), or — for a
/// browser form completing a `return_to` flow — stages a fresh ready-to-claim
/// pair and 302s it to the flow's return point, where the frontend exchanges
/// it server-to-server.
#[tracing::instrument(skip(state, body))]
pub async fn login_complete(
    State(state): State<AppState>,
    parts: Parts,
    body: LoginCompleteBody,
) -> Result<Response, StatusCode> {
    let LoginCompleteBody { request, from_form } = body;

    // This request's context feeds audit rows recorded about THIS caller (the
    // locked-login denial). The session token instead carries the browser
    // context stored on the staged row: for a browser flow, the completing
    // caller is the frontend's server, not the logging-in user.
    let ctx = ClientContext::resolve(&parts, &state.config().server);

    let current_tos = state.config().service.current_tos_version;

    let mut tx = state
        .pool()
        .begin()
        .await
        .or_internal("opening the login-completion transaction")?;

    // Consume-once: an unknown id, a wrong secret, or an expired/already-used
    // row is uniformly a 410 (no oracle distinguishing them).
    let Some(staged) =
        sql::staged_login::consume_staged(&mut tx, request.staged_id, &request.staged_secret)
            .await
            .or_internal("consuming the staged registration")?
    else {
        return Err(StatusCode::GONE);
    };

    // A browser-form completion is answered through the flow's declared return
    // point; JSON callers always get JSON (a frontend's server-to-server
    // exchange of a browser flow's pair must not be redirected).
    let redirect_to = if from_form {
        staged.return_to.clone()
    } else {
        None
    };

    // Existing user: read the login-relevant state up front. The callback only
    // stages unlocked accounts, but the account may have been locked in the
    // window since (the staging row lives for minutes): re-check under this
    // transaction and refuse like the callback would, leaving the same audit
    // trail. The consumed staging row stays consumed — once unlocked, the user
    // simply logs in again.
    let existing = if let Some(user_id) = staged.existing_user_id {
        let status = sqlx::query!(
            "select u.locked, u.tos_accepted_version, u.username, \
                    i.provider_user_id, i.provider_login \
             from tml_switchboard.users u \
             left join tml_switchboard.user_identities i \
               on i.user_id = u.subject_id and i.provider = $2 \
             where u.subject_id = $1;",
            user_id,
            staged.provider,
        )
        .fetch_one(&mut *tx)
        .await
        .or_internal(&format!("reading login state for {user_id}"))?;
        if status.locked {
            audit::emit(
                &mut tx,
                &events::LoginDeniedLocked {
                    actor: AuditSubject(user_id),
                    user: AuditSubject(user_id),
                    provider: staged.provider.clone(),
                    provider_user_id: status.provider_user_id.unwrap_or_default(),
                    login: status.provider_login.unwrap_or(status.username),
                    client_ip: ctx.ip.clone(),
                    client_port: ctx.port,
                },
            )
            .await
            .or_internal("recording the locked-login denial")?;
            tx.commit()
                .await
                .or_internal("committing the locked-login transaction")?;
            tracing::warn!("user {user_id} login completion denied: account is locked");
            return Err(StatusCode::FORBIDDEN);
        }
        Some((user_id, status))
    } else {
        None
    };

    // What the login still requires, recomputed here authoritatively (the
    // staging never stores it): a brand-new user always consents first; an
    // existing user re-consents when their accepted version went stale — which
    // covers a ToS bump that landed mid-flow, whatever the callback advertised.
    let tos_required = match &existing {
        Some((_, status)) => status.tos_accepted_version.is_none_or(|v| v < current_tos),
        None => true,
    };

    if tos_required && request.tos_version.is_none_or(|v| v != current_tos) {
        // Required consent is missing, or was given to a superseded version.
        // The presented pair is consumed; re-stage a fresh one for the client
        // to re-render and re-submit — with the longer lifetime, since a human
        // is about to read the text.
        let identity = staged
            .parse_identity()
            .transpose()
            .or_internal("decoding the staged identity")?;
        let (staged_id, staged_secret) = stage_login_row(
            &mut *tx,
            &staged.provider,
            identity.as_ref(),
            staged.existing_user_id,
            &staged.org_ids,
            staged.return_to.as_deref(),
            &ClientContext::of_staged(&staged),
            STAGED_LOGIN_LIFETIME_MINUTES,
        )
        .await?;
        tx.commit()
            .await
            .or_internal("committing the re-staged login")?;
        return staged_response(
            redirect_to.as_deref(),
            StatusCode::CONFLICT,
            staged_id,
            staged_secret,
            vec!["tos".to_string()],
            Some(current_tos),
        );
    }

    // The completion is satisfied: record the re-acceptance for an existing
    // user, or create the brand-new account at the accepted version.
    let (user_id, new_user, provider_user_id, login) = match existing {
        Some((user_id, status)) => {
            if tos_required {
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
                tracing::info!("user {user_id} accepted ToS v{current_tos}");
            }
            (
                user_id,
                false,
                status.provider_user_id.unwrap_or_default(),
                status.provider_login.unwrap_or(status.username),
            )
        }
        None => {
            let identity = staged
                .parse_identity()
                .ok_or(StatusCode::GONE)?
                .or_internal("decoding the staged identity")?;
            let user_id = sql::user::create_and_reconcile(
                &mut tx,
                &staged.provider,
                &identity,
                &staged.org_ids,
                current_tos,
            )
            .await
            .or_internal("provisioning the user")?;
            (
                user_id,
                true,
                identity.provider_user_id.clone(),
                identity.login.clone(),
            )
        }
    };

    if let Some(target) = redirect_to {
        // Browser-form completion of a `return_to` flow: hand the now
        // ready-to-claim login back through the flow's return point as a fresh
        // single-use pair, which the frontend exchanges server-to-server for
        // the token. The pair only transits the redirect, hence the short
        // lifetime.
        let (staged_id, staged_secret) = stage_login_row(
            &mut *tx,
            &staged.provider,
            None,
            Some(user_id),
            &staged.org_ids,
            staged.return_to.as_deref(),
            &ClientContext::of_staged(&staged),
            STAGED_HANDOFF_LIFETIME_MINUTES,
        )
        .await?;
        tx.commit()
            .await
            .or_internal("committing the login-completion transaction")?;
        tracing::info!("user {user_id} completed the login step; handing back to the frontend");
        return staged_response(
            Some(&target),
            StatusCode::OK,
            staged_id,
            staged_secret,
            Vec::new(),
            None,
        );
    }

    // JSON completion: mint the session token, stamped with the browser
    // context captured at the callback, and record the login marker alongside
    // it — one marker per issued token. (`new_user` records whether THIS
    // completion created the account; in the browser ToS flow the creation is
    // instead visible on the adjacent provisioning/consent events.)
    let (session_token, expires_at) = audit::transition(
        &mut tx,
        IssueSessionToken {
            user_id,
            lifetime: state.config().service.default_token_timeout,
            user_agent: staged.user_agent.clone(),
            comment: Some("interactive OAuth login".to_string()),
            created_ip: staged.created_ip.clone(),
            created_port: staged.created_port,
        },
    )
    .await
    .or_internal("issuing the session token")?;

    audit::emit(
        &mut tx,
        &events::UserLoggedIn {
            actor: AuditSubject(user_id),
            user: AuditSubject(user_id),
            provider: staged.provider.clone(),
            provider_user_id,
            login,
            new_user,
            client_ip: staged.created_ip.clone(),
            client_port: staged.created_port,
        },
    )
    .await
    .or_internal("recording the login event")?;

    tx.commit()
        .await
        .or_internal("committing the login-completion transaction")?;

    tracing::info!("user {user_id} logged in via {}", staged.provider);
    Ok(Json(LoginResponse {
        token: AuthToken::from(session_token),
        expires_at,
    })
    .into_response())
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

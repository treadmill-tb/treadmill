//! The login flow.
//!
//! `/login` renders a sign-in page listing the methods switchboard advertises
//! via `/auth/providers` (GitHub, plus any development-only mock identities).
//! Each button links to switchboard's login route with this console's landing
//! URL declared as the flow's `return_to` (switchboard validates it against its
//! allowlist). Switchboard runs the OAuth dance, stages the login, and
//! redirects the browser back to `/auth/landing` with the staged login's
//! single-use `staged_id` + `staged_secret` pair in the query. This token is
//! short lived and invalidated on use, so it's OK if this is persisted in the
//! user's browser history.
//!
//! `/auth/landing` exchanges that pair server-to-server at switchboard's `POST
//! /auth/login/complete`. A completed login yields the token, which moves into
//! the session cookie before a redirect to `/me` strips the (already-consumed)
//! `(staged_id, staged_secret)` pair from the URL. A login still requiring
//! additional information (like a ToS consent) yields a fresh `(staged_id,
//! staged_secret)` pair, rendered into a plain consent form — the pair rides
//! hidden fields, not a URL — that POSTs straight back to switchboard, which
//! answers with another redirect to `/auth/landing` carrying a ready-to-claim
//! pair: the completed branch above. No JS involved, and nothing but single-use
//! staged pairs ever transits a URL.
//!
//! `/logout` clears the cookie.

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::CookieJar;
use maud::{Markup, html};
use serde::Deserialize;
use treadmill_rs::api::switchboard::client::LoginCompleteOutcome;
use treadmill_rs::api::switchboard::{LoginCompleteRequest, LoginStagedResponse};
use uuid::Uuid;

use crate::routes::LANDING_PATH;
use crate::serve::AppState;
use crate::session::{clear_cookie, session_cookie};
use crate::views::{PageError, layout};

/// This console's landing URL — the `return_to` it declares on every login it
/// initiates. Must be allowlisted on the switchboard (implicit for the
/// embedded console).
fn landing_url(state: &AppState) -> String {
    format!(
        "{}{LANDING_PATH}",
        state.config().server.public_base_url.trim_end_matches('/')
    )
}

/// A provider `login_path` turned into an absolute switchboard login URL with
/// this console's `return_to` declared.
fn login_href(state: &AppState, login_path: &str) -> String {
    let absolute = state.switchboard(None).login_url(login_path);
    match url::Url::parse(&absolute) {
        Ok(mut url) => {
            url.query_pairs_mut()
                .append_pair("return_to", &landing_url(state));
            url.to_string()
        }
        // A malformed switchboard base URL is a config error; keep the link
        // and let switchboard reject the request visibly.
        Err(_) => absolute,
    }
}

/// `GET /login` — the sign-in page. Lists every login method the switchboard
/// offers; the visitor is sent here whenever a page needs a session and none is
/// present.
pub async fn login(State(state): State<AppState>) -> Result<Markup, PageError> {
    let client = state.switchboard(None);
    let providers = client.auth_providers().await?;
    let none_configured = providers.oauth.is_empty() && providers.mock_identities.is_empty();

    Ok(layout(
        "Sign in",
        None,
        html! {
            h1 { "Sign in" }
            section.card {
                p.muted { "You are not signed in. Choose a method to continue." }
                @if none_configured {
                    p.empty { "No sign-in methods are configured on this switchboard." }
                }
                div.login-options {
                    @for p in &providers.oauth {
                        a.button href=(login_href(&state, &p.login_path)) {
                            "Sign in with " (p.display_name)
                        }
                    }
                }
            }
            @if !providers.mock_identities.is_empty() {
                section.card.dev {
                    h2 { "Development sign-in" }
                    p.warning {
                        "Unauthenticated mock identities for local development only. "
                        "These must never be enabled in production."
                    }
                    div.login-options {
                        @for id in &providers.mock_identities {
                            a.button href=(login_href(&state, &id.login_path)) {
                                "Sign in as " (id.label)
                            }
                        }
                    }
                }
            }
        },
    ))
}

/// Query switchboard appends when redirecting a login back here: the staged
/// login's single-use pair. Absent if someone opens the page outside a flow.
#[derive(Debug, Deserialize)]
pub struct LandingQuery {
    staged_id: Option<Uuid>,
    /// The one-time secret that must accompany `staged_id`; the id alone is
    /// deliberately no capability.
    staged_secret: Option<String>,
}

/// `GET /auth/landing` — the flow's declared return point. Exchanges the
/// staged pair server-to-server at switchboard's completion endpoint and
/// branches on the outcome: session token → cookie + `/me`; consent still
/// required → the ToS form; a dead pair → a friendly restart page.
pub async fn landing(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(query): Query<LandingQuery>,
) -> Result<Response, PageError> {
    let (Some(staged_id), Some(staged_secret)) = (query.staged_id, query.staged_secret) else {
        return Ok(expired_page().into_response());
    };

    let client = state.switchboard(None);
    let outcome = client
        .login_complete(&LoginCompleteRequest {
            staged_id,
            staged_secret,
            // Declare nothing: if the login requires no consent this claims
            // it, else switchboard consumes the pair and returns a fresh one
            // alongside the version to render.
            tos_version: None,
        })
        .await;

    match outcome {
        Ok(LoginCompleteOutcome::Complete(login)) => {
            let secure = state.config().server.cookies_secure();
            let expires =
                time::OffsetDateTime::from_unix_timestamp(login.expires_at.timestamp()).ok();
            let cookie = session_cookie(login.token.encode_for_http(), secure, expires);
            Ok((jar.add(cookie), Redirect::to("/me")).into_response())
        }
        Ok(LoginCompleteOutcome::Staged(staged)) => {
            Ok(consent_page(&state, &staged).await?.into_response())
        }
        // The pair was unknown, expired, or already used (say, a reloaded
        // landing URL — it is single-use by design): offer a restart rather
        // than an error dump.
        Err(_) => Ok(expired_page().into_response()),
    }
}

/// The ToS consent form for a staged login that still requires consent. The
/// fresh pair rides hidden form fields (page body, never a URL); the form
/// POSTs straight back to switchboard's completion endpoint, which sends the
/// browser back to `/auth/landing` with a ready-to-claim pair. Declining is
/// simply abandoning the page (the staged login expires server-side).
async fn consent_page(state: &AppState, staged: &LoginStagedResponse) -> Result<Markup, PageError> {
    // Steps this console cannot render (nothing besides ToS consent exists
    // today) dead-end explicitly instead of silently mis-completing.
    if staged.required.iter().any(|step| step != "tos") {
        return Ok(layout(
            "Sign-in not supported",
            None,
            html! {
                h1 { "Sign-in not supported" }
                section.card {
                    p {
                        "Finishing this sign-in requires a step this console "
                        "does not support (" (staged.required.join(", ")) ")."
                    }
                    p.muted { a href="/login" { "Back to sign-in." } }
                }
            },
        ));
    }

    let client = state.switchboard(None);
    let tos = client.tos_info().await?;

    Ok(layout(
        "Terms of service",
        None,
        html! {
            h1 { "Terms of service" }
            section.card {
                p.muted { "Version " (tos.version) }
                p { (tos.text) }
            }
            section.card {
                p { "To finish signing in, you must accept these terms." }
                form method="post" action=(client.login_complete_url()) {
                    input type="hidden" name="staged_id" value=(staged.staged_id);
                    input type="hidden" name="staged_secret" value=(staged.staged_secret);
                    input type="hidden" name="tos_version" value=(tos.version);
                    button.button type="submit" { "Accept and continue" }
                }
                p.muted {
                    "If you do not accept, simply leave this page — nothing "
                    "is stored and the sign-in is abandoned. "
                    a href="/login" { "Back to sign-in." }
                }
            }
        },
    ))
}

/// A friendly dead-end for a missing, expired, or already-used staged pair.
fn expired_page() -> Markup {
    layout(
        "Sign-in expired",
        None,
        html! {
            h1 { "Sign-in expired" }
            section.card {
                p { "There is no sign-in in progress: the link has expired or was already used." }
                p.muted { a href="/login" { "Start over." } }
            }
        },
    )
}

/// `GET /auth/complete` — a plain Terms of Service display page. The consent
/// step of an actual login is rendered by `/auth/landing`; this page just lets
/// anyone read the terms currently in force.
pub async fn complete(State(state): State<AppState>) -> Result<Markup, PageError> {
    let tos = state.switchboard(None).tos_info().await?;
    Ok(layout(
        "Terms of service",
        None,
        html! {
            h1 { "Terms of service" }
            section.card {
                p.muted { "Version " (tos.version) }
                p { (tos.text) }
            }
        },
    ))
}

/// `GET /logout` — drop the session cookie and return to the login entry point.
pub async fn logout(jar: CookieJar) -> (CookieJar, Redirect) {
    (jar.remove(clear_cookie()), Redirect::to("/login"))
}

//! The login flow.
//!
//! `/login` renders a sign-in page listing the methods switchboard advertises
//! via `/auth/providers` (GitHub, plus any development-only mock identities).
//! Each button links to switchboard, which runs the OAuth dance and, configured
//! with `browser_success_redirect` pointing at `<console>/auth/landing`,
//! redirects back here with the freshly minted token in the query.
//! `/auth/landing` moves that token into the session cookie and sends the user
//! on to `/me`. `/logout` clears the cookie.
//!
//! When the login instead needs the completion step first (today: Terms of
//! Service consent, for a brand-new user or on a ToS version bump),
//! switchboard — configured with `browser_login_complete_redirect` pointing at
//! `<console>/auth/complete` — redirects here with the staged login's
//! `pending_id` and one-time `pending_secret` in the query. `/auth/complete`
//! renders the ToS with a plain form that POSTs the pair straight back to
//! switchboard's `/auth/login/complete`, which finishes the login and lands
//! the browser on `/auth/landing` as usual. No JS involved.
//!
//! NOTE: carrying the token through a URL query is the prototype handoff; the
//! hardened design (one-time code + back-channel exchange) is tracked in the
//! repo-root `TODOS.md`.

use axum::extract::{Query, State};
use axum::response::Redirect;
use axum_extra::extract::cookie::CookieJar;
use chrono::DateTime;
use maud::{Markup, html};
use serde::Deserialize;
use uuid::Uuid;

use crate::serve::AppState;
use crate::session::{clear_cookie, session_cookie};
use crate::views::{PageError, layout};

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
                        a.button href=(client.login_url(&p.login_path)) {
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
                            a.button href=(client.login_url(&id.login_path)) {
                                "Sign in as " (id.label)
                            }
                        }
                    }
                }
            }
        },
    ))
}

/// Query switchboard appends to the landing redirect after a successful login.
#[derive(Debug, Deserialize)]
pub struct LandingQuery {
    token: String,
    /// RFC 3339 token expiry; used to bound the cookie's lifetime.
    expires_at: Option<String>,
}

/// `GET /auth/landing` — store the issued token in the session cookie, then go
/// to the profile page.
pub async fn landing(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(query): Query<LandingQuery>,
) -> (CookieJar, Redirect) {
    let secure = state.config().server.cookies_secure();
    let expires = query
        .expires_at
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .and_then(|dt| time::OffsetDateTime::from_unix_timestamp(dt.timestamp()).ok());
    let cookie = session_cookie(query.token, secure, expires);
    (jar.add(cookie), Redirect::to("/me"))
}

/// Query switchboard appends when redirecting a login to the completion page.
#[derive(Debug, Deserialize)]
pub struct CompleteQuery {
    /// The staged login to finish; absent if someone opens the page outside a
    /// login flow (we then render the ToS without a completion form).
    pending_id: Option<Uuid>,
    /// The one-time secret that must accompany `pending_id`; the id alone is
    /// deliberately no capability.
    pending_secret: Option<String>,
}

/// `GET /auth/complete` — the login-completion page (today: ToS consent).
/// Fetches the current ToS from switchboard and renders it with a form that
/// POSTs the pending pair — plus the version of the text actually shown —
/// directly to switchboard's `/auth/login/complete`; on success switchboard
/// sends the browser back to `/auth/landing` with the token, completing the
/// login. Declining is simply abandoning the page (the staged login expires
/// server-side).
pub async fn complete(
    State(state): State<AppState>,
    Query(query): Query<CompleteQuery>,
) -> Result<Markup, PageError> {
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
                @if let (Some(pending_id), Some(pending_secret)) =
                    (query.pending_id, &query.pending_secret)
                {
                    p { "To finish signing in, you must accept these terms." }
                    form method="post" action=(client.login_complete_url()) {
                        input type="hidden" name="pending_id" value=(pending_id);
                        input type="hidden" name="pending_secret" value=(pending_secret);
                        input type="hidden" name="tos_version" value=(tos.version);
                        button.button type="submit" { "Accept and continue" }
                    }
                    p.muted {
                        "If you do not accept, simply leave this page — nothing "
                        "is stored and the sign-in is abandoned. "
                        a href="/login" { "Back to sign-in." }
                    }
                } @else {
                    p.empty {
                        "There is no sign-in awaiting consent (the link may have "
                        "expired). " a href="/login" { "Start over." }
                    }
                }
            }
        },
    ))
}

/// `GET /logout` — drop the session cookie and return to the login entry point.
pub async fn logout(jar: CookieJar) -> (CookieJar, Redirect) {
    (jar.remove(clear_cookie()), Redirect::to("/login"))
}

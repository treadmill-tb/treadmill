//! The login flow.
//!
//! `/login` bounces the browser to switchboard's GitHub login. Switchboard runs
//! the OAuth dance and, configured with `browser_success_redirect` pointing at
//! `<console>/auth/landing`, redirects back here with the freshly minted token
//! in the query. `/auth/landing` moves that token into the session cookie and
//! sends the user on to `/me`. `/logout` clears the cookie.
//!
//! NOTE: carrying the token through a URL query is the prototype handoff; the
//! hardened design (one-time code + back-channel exchange) is tracked in the
//! repo-root `TODOS.md`.

use axum::extract::{Query, State};
use axum::response::Redirect;
use axum_extra::extract::cookie::CookieJar;
use chrono::DateTime;
use serde::Deserialize;

use crate::serve::AppState;
use crate::session::{clear_cookie, session_cookie};

/// `GET /login` — begin login by redirecting to switchboard.
pub async fn login(State(state): State<AppState>) -> Redirect {
    Redirect::to(&state.switchboard(None).github_login_url())
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

/// `GET /logout` — drop the session cookie and return to the login entry point.
pub async fn logout(jar: CookieJar) -> (CookieJar, Redirect) {
    (jar.remove(clear_cookie()), Redirect::to("/login"))
}

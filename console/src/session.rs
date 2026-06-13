//! The browser session: a switchboard bearer token carried in an `HttpOnly`
//! cookie.
//!
//! The console keeps no session state. The cookie holds the raw switchboard
//! token (base64, as the API encodes it); switchboard re-validates it on every
//! request, so the cookie needs no signing or encryption — only `HttpOnly`,
//! `SameSite=Lax`, and `Secure` on an HTTPS origin. The [`Session`] extractor
//! turns a present cookie into a ready-to-use [`SwitchboardClient`]; an absent
//! cookie short-circuits the handler with a redirect to `/login`.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use treadmill_rs::api::switchboard::client::SwitchboardClient;

use crate::serve::AppState;

/// Name of the cookie holding the switchboard bearer token.
pub const SESSION_COOKIE: &str = "tml_session";

/// An authenticated browser session, extracted from the request cookie.
pub struct Session {
    /// A switchboard client bound to this session's token.
    pub client: SwitchboardClient,
    /// The raw bearer token, kept so handlers can flag the "current" session.
    pub token: String,
}

/// Rejection for [`Session`]: there is no usable session cookie, so the visitor
/// is sent to begin a login.
pub struct NotLoggedIn;

impl IntoResponse for NotLoggedIn {
    fn into_response(self) -> Response {
        Redirect::to("/login").into_response()
    }
}

impl FromRequestParts<AppState> for Session {
    type Rejection = NotLoggedIn;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let jar = CookieJar::from_headers(&parts.headers);
        let token = jar
            .get(SESSION_COOKIE)
            .map(|c| c.value().to_string())
            .ok_or(NotLoggedIn)?;
        let client = state.switchboard(Some(token.clone()));
        Ok(Session { client, token })
    }
}

/// Build the session cookie carrying `token`. `secure` follows the public
/// origin's scheme; `expires` mirrors the token's own expiry so the browser
/// drops it when switchboard would stop honouring it.
pub fn session_cookie(
    token: String,
    secure: bool,
    expires: Option<time::OffsetDateTime>,
) -> Cookie<'static> {
    let mut builder = Cookie::build((SESSION_COOKIE, token))
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(secure)
        .path("/");
    if let Some(expires) = expires {
        builder = builder.expires(expires);
    }
    builder.build()
}

/// A removal cookie that clears the session on the browser.
pub fn clear_cookie() -> Cookie<'static> {
    // Path must match the one set above for the removal to take effect.
    let mut cookie = Cookie::from(SESSION_COOKIE);
    cookie.set_path("/");
    cookie
}

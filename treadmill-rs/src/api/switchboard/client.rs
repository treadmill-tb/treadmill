//! A typed HTTP client for the switchboard REST API.
//!
//! Every method returns the *same* `api::switchboard::*` types the switchboard
//! serializes from, so a change to a request/response shape — or to an endpoint
//! method/path defined here — is a compile error in every consumer (the web
//! console, an eventual CLI). That closes both the payload gap and the route
//! gap with no codegen and no spec to drift.
//!
//! Enabled by the `client` cargo feature (off by default, so non-HTTP consumers
//! don't pull `reqwest`).

use uuid::Uuid;

use crate::api::switchboard::WhoAmIResponse;
use crate::api::switchboard::audit::AuditFeedResponse;
use crate::api::switchboard::users::{PublicUserProfile, SelfUserProfile, SessionInfo};

/// All the ways a switchboard API call can fail, from the caller's point of
/// view. `Unauthorized` is split out from other error statuses because callers
/// (e.g. the web console) act on it specifically: drop the session and ask the
/// user to log in again.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The request never produced a valid HTTP response (DNS, TLS, connect,
    /// timeout, body decode, …).
    #[error("switchboard request failed: {0}")]
    Transport(#[from] reqwest::Error),
    /// `401 Unauthorized`: the bearer token is missing, invalid, or expired.
    #[error("unauthorized: the session token is missing, invalid, or expired")]
    Unauthorized,
    /// Any other non-success status, with the response body for context.
    #[error("switchboard returned HTTP {status}: {body}")]
    Status { status: u16, body: String },
}

/// A handle to one switchboard instance, optionally carrying a bearer token for
/// authenticated calls.
///
/// `base_url` is the switchboard origin (e.g. `https://sb.example`); the
/// `/api/v1` prefix is added internally. Cheap to clone (wraps a `reqwest`
/// client, which is itself an `Arc` internally).
#[derive(Clone)]
pub struct SwitchboardClient {
    http: reqwest::Client,
    /// Origin without a trailing slash, e.g. `https://sb.example`.
    base_url: String,
    /// The bearer token to send, already encoded for HTTP (base64), if any.
    token: Option<String>,
}

impl SwitchboardClient {
    /// Build a client for `base_url`. Pass `token = None` for anonymous calls
    /// (e.g. building the login URL) or `Some(token)` for authenticated ones.
    pub fn new(base_url: impl Into<String>, token: Option<String>) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Self {
            http: reqwest::Client::new(),
            base_url,
            token,
        }
    }

    /// The browser entry point for interactive GitHub login. Not a JSON API
    /// call — the console redirects the user's browser here, and switchboard
    /// carries them through the OAuth flow.
    pub fn github_login_url(&self) -> String {
        format!("{}/api/v1/auth/github/login", self.base_url)
    }

    /// `GET /auth/whoami` — the identity behind the current token.
    pub async fn whoami(&self) -> Result<WhoAmIResponse, ClientError> {
        self.get_json("/api/v1/auth/whoami").await
    }

    /// `GET /users/me` — the caller's own profile (emails, groups, lock state).
    pub async fn get_me(&self) -> Result<SelfUserProfile, ClientError> {
        self.get_json("/api/v1/users/me").await
    }

    /// `GET /users/{id}` — the public subset of another user's profile.
    pub async fn get_user(&self, user_id: Uuid) -> Result<PublicUserProfile, ClientError> {
        self.get_json(&format!("/api/v1/users/{user_id}")).await
    }

    /// `GET /users/me/tokens` — the caller's sessions/tokens.
    pub async fn list_my_tokens(&self) -> Result<Vec<SessionInfo>, ClientError> {
        self.get_json("/api/v1/users/me/tokens").await
    }

    /// `GET /users/{id}/events` — a user's audit feed (self/admin only).
    pub async fn user_events(&self, user_id: Uuid) -> Result<AuditFeedResponse, ClientError> {
        self.get_json(&format!("/api/v1/users/{user_id}/events"))
            .await
    }

    /// Issue an authenticated `GET` for `path` and deserialize the JSON body,
    /// mapping `401` to [`ClientError::Unauthorized`] and any other non-success
    /// status to [`ClientError::Status`].
    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let mut req = self.http.get(format!("{}{path}", self.base_url));
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await?;

        let status = resp.status();
        if status.is_success() {
            return Ok(resp.json::<T>().await?);
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ClientError::Unauthorized);
        }
        let body = resp.text().await.unwrap_or_default();
        Err(ClientError::Status {
            status: status.as_u16(),
            body,
        })
    }
}

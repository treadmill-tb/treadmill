//! OAuth login providers.
//!
//! Each external identity provider (GitHub, and others later) is hidden behind
//! [`OAuthProvider`] so that the login routes and the user-provisioning logic
//! are provider-agnostic, and so tests can target a fake provider/server.

pub mod github;

use async_trait::async_trait;
use thiserror::Error;

/// An access token obtained from an OAuth provider, used to call its API.
#[derive(Clone)]
pub struct OAuthAccessToken(pub String);

/// Identity information fetched from a provider after a successful login.
#[derive(Debug, Clone)]
pub struct ExternalIdentity {
    /// The provider's STABLE numeric user id, as text. Never the login handle
    /// (handles are renameable/reusable and would mis-link accounts over time).
    pub provider_user_id: String,
    /// The user's current login/handle on the provider. Used only as the
    /// suggested internal username and for display.
    pub login: String,
    /// The user's display name, if the provider exposes one.
    pub full_name: Option<String>,
    /// URL of the user's avatar, if any.
    pub avatar_url: Option<String>,
    /// ONLY verified email addresses. Used to link a login to an existing user.
    pub verified_emails: Vec<String>,
}

/// Things that can go wrong talking to an OAuth provider.
#[derive(Debug, Error)]
pub enum OAuthError {
    /// The provider is misconfigured (bad URL, client build failure).
    #[error("invalid provider configuration: {0}")]
    Config(String),
    /// Exchanging the authorization code for an access token failed.
    #[error("token exchange failed: {0}")]
    Exchange(String),
    /// A call to the provider's REST API failed.
    #[error("provider API request failed: {0}")]
    Api(String),
}

/// A single external identity provider.
#[async_trait]
pub trait OAuthProvider {
    /// Provider name; matches `user_identities.provider`, e.g. `"github"`.
    fn name(&self) -> &'static str;

    /// Build the authorization URL the user is redirected to, plus the CSRF
    /// state token that must be persisted and echoed back on callback.
    fn authorize(&self) -> Result<(String, oauth2::CsrfToken), OAuthError>;

    /// Exchange an authorization code for an access token.
    async fn exchange(&self, code: String) -> Result<OAuthAccessToken, OAuthError>;

    /// Fetch the user's identity (profile + verified emails).
    async fn fetch_identity(
        &self,
        token: &OAuthAccessToken,
    ) -> Result<ExternalIdentity, OAuthError>;

    /// Fetch the provider org ids (as text) the user currently belongs to. Feeds
    /// auto-group reconciliation. Best-effort: providers without an org concept
    /// (or insufficient scope) return an empty list.
    async fn fetch_org_ids(&self, token: &OAuthAccessToken) -> Result<Vec<String>, OAuthError>;
}

//! OAuth login providers.
//!
//! Each external identity provider (GitHub, and others later) is hidden behind
//! [`OAuthProvider`] so that the login routes and the user-provisioning logic
//! are provider-agnostic, and so tests can target a fake provider/server.

pub mod github;
pub mod mock;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use thiserror::Error;

/// An access token obtained from an OAuth provider, used to call its API.
#[derive(Clone)]
pub struct OAuthAccessToken(pub String);

/// Email addresses fetched from an OAuth provider.
///
/// `verified` means that the OAuth provider promises to have verified that this
/// email address belongs to the given user *AND* that we trust this information
/// for the purposes of linking other OAuth handles to a given user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Email<'a> {
    /// The email address.
    ///
    /// This is a [`Cow`] to allow it to be used in constants for test fixtures.
    pub address: Cow<'a, str>,
    /// Whether the OAuth provider claims (and we trust) that this email address
    /// has been verified to belong to the respective user.
    pub verified: bool,
}

/// Identity information fetched from a provider after a successful login.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Email addresses of the user (both verified & unverified).
    pub emails: Vec<Email<'static>>,
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
    ///
    /// `query` carries the login request's query parameters. External providers
    /// (GitHub) ignore them; the dev-only mock provider reads `identity` from
    /// here to decide which canned identity to issue.
    fn authorize(
        &self,
        query: &HashMap<String, String>,
    ) -> Result<(String, oauth2::CsrfToken), OAuthError>;

    /// Exchange an authorization code for an access token.
    async fn exchange(&self, code: String) -> Result<OAuthAccessToken, OAuthError>;

    /// Fetch the user's identity (profile + verified emails).
    async fn fetch_identity(
        &self,
        token: &OAuthAccessToken,
    ) -> Result<ExternalIdentity, OAuthError>;

    /// Fetch the provider org ids (as text) the user currently belongs to. Feeds
    /// auto-group reconciliation and, at registration, org-based admission.
    ///
    /// A successful call returning no active orgs is `Ok(vec![])`; a genuine call
    /// failure (network error, non-success status, insufficient scope) surfaces as
    /// `Err`. Providers without an org concept return `Ok(vec![])`.
    ///
    /// The Ok/Err distinction is load-bearing for admission: on the new-user path
    /// the callback treats `Err` as a fail-closed (retryable) deny rather than
    /// admitting nobody. The existing-user path keeps it best-effort
    /// (`.unwrap_or_default()`), for auto-group reconciliation only.
    async fn fetch_org_ids(&self, token: &OAuthAccessToken) -> Result<Vec<String>, OAuthError>;
}

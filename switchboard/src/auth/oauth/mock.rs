//! Development-only mock OAuth provider.
//!
//! This provider relies on **no external web service**. It exposes a small set
//! of built-in, pre-defined identities; the caller chooses one via the
//! `?identity=<key>` parameter on the login route. [`authorize`] then redirects
//! the browser straight back to the switchboard's own callback (the chosen
//! identity riding along as the opaque `code`), so the *entire* real login path
//! — CSRF flow, code exchange, user provisioning, token issuance, audit — runs
//! unchanged; only the provider's network I/O is faked.
//!
//! DANGER: this is a deliberate authentication bypass. It mints a valid session
//! for any built-in identity with no proof of who the caller is. It is gated
//! only by `[oauth.mock] enabled = true` and must never run in production. See
//! [`MockOAuthConfig`](crate::config::MockOAuthConfig).
//!
//! [`authorize`]: OAuthProvider::authorize

use super::{Email, ExternalIdentity, OAuthAccessToken, OAuthError, OAuthProvider};
use async_trait::async_trait;
use oauth2::CsrfToken;
use std::borrow::Cow;
use std::collections::HashMap;

/// One built-in mock identity. `key` doubles as the stable provider user id, so
/// re-logging in as the same key always resolves to the same local user.
pub struct MockIdentity {
    /// Selector used in `?identity=` and stored as the provider user id.
    pub key: &'static str,
    /// Login/handle, used as the suggested username and for display.
    pub login: &'static str,
    /// Display name.
    pub full_name: &'static str,
    /// User emails, can be verified or unverified.
    pub emails: &'static [Email<'static>],
    /// Whether logging in as this identity grants global admin (see
    /// [`OAuthProvider::grants_global_admin`]).
    pub admin: bool,
}

/// The fixed roster. `alice` is the admin; `bob` and `carol` are plain users.
pub const MOCK_IDENTITIES: &[MockIdentity] = &[
    MockIdentity {
        key: "alice",
        login: "alice",
        full_name: "Alice Example",
        emails: &[
            Email {
                address: Cow::Borrowed("alice@example.test"),
                verified: true,
            },
            Email {
                address: Cow::Borrowed("alice-alt@example.org"),
                verified: false,
            },
        ],
        admin: true,
    },
    MockIdentity {
        key: "bob",
        login: "bob",
        full_name: "Bob Example",
        emails: &[Email {
            address: Cow::Borrowed("bob@example.test"),
            verified: true,
        }],
        admin: false,
    },
    MockIdentity {
        key: "carol",
        login: "carol",
        full_name: "Carol Example",
        emails: &[Email {
            address: Cow::Borrowed("carol@example.test"),
            verified: false,
        }],
        admin: false,
    },
];

/// Look up a built-in identity by its `key`.
fn lookup(key: &str) -> Option<&'static MockIdentity> {
    MOCK_IDENTITIES.iter().find(|i| i.key == key)
}

/// The development-only mock provider. Stateless: the roster is a compile-time
/// constant.
pub struct MockProvider;

#[async_trait]
impl OAuthProvider for MockProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn authorize(
        &self,
        query: &HashMap<String, String>,
    ) -> Result<(String, CsrfToken), OAuthError> {
        let key = query.get("identity").ok_or_else(|| {
            OAuthError::Config("mock login requires an `identity` query parameter".to_string())
        })?;
        let id = lookup(key)
            .ok_or_else(|| OAuthError::Config(format!("unknown mock identity {key:?}")))?;

        let csrf = CsrfToken::new_random();
        // No external service: redirect straight to our own callback. The chosen
        // identity rides back as the opaque `code`. Both the key (alphanumeric)
        // and the CSRF token (url-safe base64) are URL-safe as-is.
        let url = format!(
            "/api/v1/auth/mock/callback?code={}&state={}",
            id.key,
            csrf.secret(),
        );
        Ok((url, csrf))
    }

    async fn exchange(&self, code: String) -> Result<OAuthAccessToken, OAuthError> {
        // The "code" is just the identity key; validate it round-tripped intact.
        lookup(&code)
            .ok_or_else(|| OAuthError::Exchange(format!("unknown mock identity {code:?}")))?;
        Ok(OAuthAccessToken(code))
    }

    async fn fetch_identity(
        &self,
        token: &OAuthAccessToken,
    ) -> Result<ExternalIdentity, OAuthError> {
        let id = lookup(&token.0)
            .ok_or_else(|| OAuthError::Api(format!("unknown mock identity {:?}", token.0)))?;
        Ok(ExternalIdentity {
            provider_user_id: id.key.to_string(),
            login: id.login.to_string(),
            full_name: Some(id.full_name.to_string()),
            avatar_url: None,
            emails: id.emails.into(),
        })
    }

    async fn fetch_org_ids(&self, _token: &OAuthAccessToken) -> Result<Vec<String>, OAuthError> {
        // The mock provider has no org concept; admin is granted directly via
        // `grants_global_admin`, not through org-driven auto-groups.
        Ok(Vec::new())
    }
}

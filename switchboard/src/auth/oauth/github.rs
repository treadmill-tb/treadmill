//! GitHub OAuth provider.

use super::{ExternalIdentity, OAuthAccessToken, OAuthError, OAuthProvider};
use crate::config::GitHubOAuthConfig;
use async_trait::async_trait;
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet, EndpointSet,
    RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use serde::Deserialize;

/// The `oauth2` client typestate once auth + token + redirect URIs are set
/// (device-authorization, introspection, and revocation endpoints stay unset).
type GithubOAuthClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

pub struct GithubProvider {
    client: GithubOAuthClient,
    http: reqwest::Client,
    api_base_url: String,
    scopes: Vec<String>,
}

impl GithubProvider {
    pub fn from_config(cfg: &GitHubOAuthConfig) -> Result<Self, OAuthError> {
        let mkurl = |s: &str, what: &str| {
            AuthUrl::new(s.to_string()).map_err(|e| OAuthError::Config(format!("{what}: {e}")))
        };
        let client = BasicClient::new(ClientId::new(cfg.client_id.clone()))
            .set_client_secret(ClientSecret::new(cfg.client_secret.clone()))
            .set_auth_uri(mkurl(&cfg.auth_url, "auth_url")?)
            .set_token_uri(
                TokenUrl::new(cfg.token_url.clone())
                    .map_err(|e| OAuthError::Config(format!("token_url: {e}")))?,
            )
            .set_redirect_uri(
                RedirectUrl::new(cfg.redirect_url.clone())
                    .map_err(|e| OAuthError::Config(format!("redirect_url: {e}")))?,
            );

        // Redirects disabled per the oauth2 crate's SSRF guidance: the token
        // endpoint response must not be chased to an attacker-controlled host.
        let http = reqwest::ClientBuilder::new()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("treadmill-switchboard")
            .build()
            .map_err(|e| OAuthError::Config(format!("http client: {e}")))?;

        Ok(Self {
            client,
            http,
            api_base_url: cfg.api_base_url.trim_end_matches('/').to_string(),
            scopes: cfg.scopes.clone(),
        })
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        token: &OAuthAccessToken,
        path: &str,
    ) -> Result<T, OAuthError> {
        let url = format!("{}{}", self.api_base_url, path);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token.0)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .map_err(|e| OAuthError::Api(format!("GET {path}: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(OAuthError::Api(format!("GET {path} -> {status}")));
        }
        resp.json::<T>()
            .await
            .map_err(|e| OAuthError::Api(format!("GET {path} decode: {e}")))
    }
}

#[derive(Deserialize)]
struct GhUser {
    id: i64,
    login: String,
    name: Option<String>,
    avatar_url: Option<String>,
}

#[derive(Deserialize)]
struct GhEmail {
    email: String,
    verified: bool,
}

#[derive(Deserialize)]
struct GhOrg {
    id: i64,
}

#[derive(Deserialize)]
struct GhMembership {
    state: String,
    organization: GhOrg,
}

#[async_trait]
impl OAuthProvider for GithubProvider {
    fn name(&self) -> &'static str {
        "github"
    }

    fn authorize(
        &self,
        _query: &std::collections::HashMap<String, String>,
    ) -> Result<(String, CsrfToken), OAuthError> {
        let mut req = self.client.authorize_url(CsrfToken::new_random);
        for scope in &self.scopes {
            req = req.add_scope(Scope::new(scope.clone()));
        }
        let (url, csrf) = req.url();
        Ok((url.to_string(), csrf))
    }

    async fn exchange(&self, code: String) -> Result<OAuthAccessToken, OAuthError> {
        let token = self
            .client
            .exchange_code(AuthorizationCode::new(code))
            .request_async(&self.http)
            .await
            .map_err(|e| OAuthError::Exchange(e.to_string()))?;
        Ok(OAuthAccessToken(token.access_token().secret().clone()))
    }

    async fn fetch_identity(
        &self,
        token: &OAuthAccessToken,
    ) -> Result<ExternalIdentity, OAuthError> {
        let user: GhUser = self.get_json(token, "/user").await?;
        // Best-effort: an account may not have granted user:email; without it we
        // simply provision without linkable emails rather than failing login.
        let emails: Vec<GhEmail> = self
            .get_json(token, "/user/emails")
            .await
            .unwrap_or_default();
        let verified_emails = emails
            .into_iter()
            .filter(|e| e.verified)
            .map(|e| e.email)
            .collect();
        Ok(ExternalIdentity {
            provider_user_id: user.id.to_string(),
            login: user.login,
            full_name: user.name,
            avatar_url: user.avatar_url,
            verified_emails,
        })
    }

    async fn fetch_org_ids(&self, token: &OAuthAccessToken) -> Result<Vec<String>, OAuthError> {
        // Best-effort: requires the read:org scope; absent it, auto-groups simply
        // do not apply for this login.
        let memberships: Vec<GhMembership> = self
            .get_json(token, "/user/memberships/orgs?state=active")
            .await
            .unwrap_or_default();
        Ok(memberships
            .into_iter()
            .filter(|m| m.state == "active")
            .map(|m| m.organization.id.to_string())
            .collect())
    }
}

//! GitHub OAuth provider.

use super::{Email, ExternalIdentity, OAuthAccessToken, OAuthError, OAuthProvider};
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

/// The GitHub OAuth scope that makes a user's *private* org memberships visible
/// via `/user/memberships/orgs`. Without it that endpoint reports only orgs the
/// user has made public, so org-based admission would silently fail for members
/// with private membership. Requested unconditionally (see [`authorize`]),
/// never left to operator-configured `scopes`.
///
/// [`authorize`]: OAuthProvider::authorize
const REQUIRED_ORG_SCOPE: &str = "read:org";

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
        // Always request `read:org` alongside the configured scopes, deduped
        // (see `REQUIRED_ORG_SCOPE`).
        for scope in self
            .scopes
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(REQUIRED_ORG_SCOPE))
            .filter(|s| !s.is_empty())
            .collect::<std::collections::BTreeSet<_>>()
        {
            req = req.add_scope(Scope::new(scope.to_string()));
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
        let emails: Vec<GhEmail> = self.get_json(token, "/user/emails").await?;
        Ok(ExternalIdentity {
            provider_user_id: user.id.to_string(),
            login: user.login,
            full_name: user.name,
            avatar_url: user.avatar_url,
            emails: emails
                .into_iter()
                .map(
                    |GhEmail {
                         email: address,
                         verified,
                     }| Email {
                        address: address.into(),
                        verified,
                    },
                )
                .collect(),
        })
    }

    async fn fetch_org_ids(&self, token: &OAuthAccessToken) -> Result<Vec<String>, OAuthError> {
        // A failed fetch propagates as `Err`, never a silent `Ok(vec![])`: the
        // new-user admission path relies on this to fail closed (see the trait doc).
        let memberships: Vec<GhMembership> = self
            .get_json(token, "/user/memberships/orgs?state=active")
            .await?;
        Ok(memberships
            .into_iter()
            .filter(|m| m.state == "active")
            .map(|m| m.organization.id.to_string())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GitHubOAuthConfig;
    use url::Url;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn config(api_base_url: &str, scopes: Vec<String>) -> GitHubOAuthConfig {
        GitHubOAuthConfig {
            client_id: "test-client".to_string(),
            client_secret: "test-secret".to_string(),
            redirect_url: "http://localhost/api/v1/auth/github/callback".to_string(),
            auth_url: "https://github.com/login/oauth/authorize".to_string(),
            token_url: "https://github.com/login/oauth/access_token".to_string(),
            api_base_url: api_base_url.to_string(),
            scopes,
        }
    }

    /// The space-separated `scope` values carried by an authorize URL.
    fn scopes_in(url: &str) -> Vec<String> {
        Url::parse(url)
            .expect("valid authorize URL")
            .query_pairs()
            .find(|(k, _)| k == "scope")
            .map(|(_, v)| v.split(' ').map(str::to_string).collect())
            .unwrap_or_default()
    }

    /// `read:org` is load-bearing for org-based admission and must be requested
    /// even when the operator's `scopes` config omits it.
    #[test]
    fn authorize_url_always_requests_read_org_even_if_config_omits_it() {
        let provider = GithubProvider::from_config(&config(
            "https://api.github.com",
            vec!["read:user".to_string(), "user:email".to_string()],
        ))
        .expect("provider builds");
        let (url, _csrf) = provider
            .authorize(&std::collections::HashMap::new())
            .expect("authorize url");
        let scopes = scopes_in(&url);
        assert!(
            scopes.iter().any(|s| s == REQUIRED_ORG_SCOPE),
            "read:org must always be present; got {scopes:?}"
        );
    }

    /// Requesting `read:org` unconditionally must not duplicate it when the
    /// operator's config already lists it.
    #[test]
    fn authorize_url_does_not_duplicate_read_org() {
        let provider = GithubProvider::from_config(&config(
            "https://api.github.com",
            vec![
                "read:user".to_string(),
                "user:email".to_string(),
                "read:org".to_string(),
            ],
        ))
        .expect("provider builds");
        let (url, _csrf) = provider
            .authorize(&std::collections::HashMap::new())
            .expect("authorize url");
        let scopes = scopes_in(&url);
        assert_eq!(
            scopes.iter().filter(|s| *s == REQUIRED_ORG_SCOPE).count(),
            1,
            "read:org must appear exactly once; got {scopes:?}"
        );
    }

    /// A failed org fetch (here a 500) must surface as `Err`, not be swallowed to
    /// `Ok(empty)` — the new-user admission path relies on this to fail closed.
    #[tokio::test]
    async fn fetch_org_ids_propagates_server_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/memberships/orgs"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let provider =
            GithubProvider::from_config(&config(&server.uri(), vec![])).expect("provider builds");
        let result = provider
            .fetch_org_ids(&OAuthAccessToken("t".to_string()))
            .await;
        assert!(
            matches!(result, Err(OAuthError::Api(_))),
            "a 500 from the org endpoint must be Err, not Ok(empty); got {result:?}"
        );
    }

    /// A *successful* call returning no active orgs stays `Ok(vec![])` — genuine
    /// empty membership must be distinguishable from a failure.
    #[tokio::test]
    async fn fetch_org_ids_empty_membership_is_ok_empty() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/memberships/orgs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        let provider =
            GithubProvider::from_config(&config(&server.uri(), vec![])).expect("provider builds");
        let orgs = provider
            .fetch_org_ids(&OAuthAccessToken("t".to_string()))
            .await
            .expect("a successful empty response is Ok");
        assert!(orgs.is_empty(), "expected no orgs; got {orgs:?}");
    }
}

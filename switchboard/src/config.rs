use anyhow::Context;
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;
use treadmill_rs::util::chrono::duration as human_duration;

#[derive(Debug, Clone, Deserialize)]
pub struct SwitchboardConfig {
    /// Configuration for connecting to PostgreSQL server.
    pub database: DatabaseConfig,
    /// Configuration of the HTTP server.
    pub server: ServerConfig,
    /// Configuration of the Switchboard service.
    pub service: ServiceConfig,
    /// OAuth login providers. Optional: a deployment without any provider
    /// configured simply cannot issue interactive logins.
    #[serde(default)]
    pub oauth: OAuthConfig,
    /// Log streaming via NATS/JetStream. Absent (the default) disables the
    /// feature: jobs dispatch without a log-streaming destination and the
    /// read-token API is unavailable.
    #[serde(default)]
    pub log_streaming: Option<LogStreamingConfig>,
}

impl SwitchboardConfig {
    /// Whether `url` is an allowed interactive-login `return_to` target: an
    /// exact match against [`OAuthConfig::return_to_allowlist`].
    ///
    /// The staged pair the callback appends to `return_to` is a token-minting
    /// capability; every `return_to` MUST pass this check, both when a flow is
    /// initiated and again at the callback (the config may change mid-flow).
    pub fn return_to_allowed(&self, url: &str) -> bool {
        self.oauth.return_to_allowlist.iter().any(|a| a == url)
    }
}

/// Configuration for host/supervisor log streaming over NATS + JetStream.
///
/// Supervisor console output is published to a per-job JetStream stream; the
/// switchboard mints short-lived, per-job **bearer** user JWTs that the NATS
/// server validates against the scope it grants. See
/// `doc/log-streaming-plan.md`.
#[derive(Debug, Clone, Deserialize)]
pub struct LogStreamingConfig {
    /// NATS client URL the supervisors and read clients connect to (e.g.
    /// `nats://nats.example:4222`). Handed to supervisors in `StartJobMessage`
    /// and returned to read clients alongside their token. Non-secret.
    pub nats_url: String,
    /// NATS **WebSocket** URL (e.g. `wss://nats.example:443`), returned to
    /// read clients instead of `nats_url` when set. Browsers cannot speak the
    /// plain TCP client protocol, so any deployment serving the web console
    /// must point this at the server's `websocket` listener. Non-secret.
    #[serde(default)]
    pub websocket_url: Option<String>,
    /// JetStream domain, if the server is configured with one. Usually unset.
    #[serde(default)]
    pub jetstream_domain: Option<String>,
    /// NATS account **signing seed** (an nkey account seed, `SA…`) used to sign
    /// the per-job user JWTs. The matching account public key is derived from
    /// this seed, so it doubles as the JWT issuer.
    ///
    /// SECRET — supply via `TML_LOGSTREAMING__ACCOUNT_SEED`, never on-disk
    /// config (project convention). Unrelated to the opaque API token in
    /// `auth/token.rs`.
    pub account_seed: String,
}

/// OAuth login provider configuration. Each provider is independently optional.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OAuthConfig {
    /// GitHub login, if configured.
    pub github: Option<GitHubOAuthConfig>,
    /// Development-only mock login, if configured. See [`MockOAuthConfig`].
    pub mock: Option<MockOAuthConfig>,
    /// The exact URLs a browser frontend may declare as its `return_to` when
    /// initiating a login (`GET /auth/{provider}/login?return_to=…`), e.g.
    /// `https://console.example/auth/landing`. One entry per frontend; matched
    /// by exact string comparison (no prefix or origin matching), so this list
    /// carries no open-redirect surface.
    ///
    /// The callback `302`-redirects a flow with a `return_to` there, carrying
    /// the single-use `?staged_id=…&staged_secret=…` pair, which the frontend
    /// exchanges server-to-server at `POST /auth/login/complete` for the
    /// session token. The pair is a token-minting capability, so an
    /// unvalidated `return_to` would hand account takeover to an
    /// attacker-chosen URL — never bypass this list. Flows that declare no
    /// `return_to` (programmatic clients) receive JSON instead; the two styles
    /// coexist per-request on one deployment.
    #[serde(default)]
    pub return_to_allowlist: Vec<String>,
}

/// Configuration for GitHub OAuth login.
///
/// The endpoint URLs default to GitHub's production endpoints but are
/// overridable so tests can point the flow at a local mock server, exercising
/// the full authorization-code exchange without a third-party dependency.
#[derive(Debug, Clone, Deserialize)]
pub struct GitHubOAuthConfig {
    /// OAuth app client id.
    pub client_id: String,
    /// OAuth app client secret.
    pub client_secret: String,
    /// Absolute URL the provider redirects back to (must match the OAuth app's
    /// configured callback), e.g. `https://switchboard.example/api/v1/auth/github/callback`.
    pub redirect_url: String,
    /// Authorization endpoint (where the user is sent to approve access).
    #[serde(default = "default_github_auth_url")]
    pub auth_url: String,
    /// Token endpoint (where the authorization code is exchanged for a token).
    #[serde(default = "default_github_token_url")]
    pub token_url: String,
    /// Base URL of the provider's REST API (used to fetch the user profile and
    /// verified emails). A trailing slash, if present, is ignored.
    #[serde(default = "default_github_api_base_url")]
    pub api_base_url: String,
}

/// Configuration for the **development-only** mock OAuth provider.
///
/// The mock provider mints a valid session token for one of a small set of
/// built-in, pre-defined identities with **no authentication whatsoever** — it
/// is a deliberate auth bypass for local development and testing, and relies on
/// no external web service. It is OFF unless `enabled = true`.
///
/// DANGER: enabling this on a reachable deployment turns it into an open door.
/// There is no compile-time guard; the only protection is this flag, a loud
/// warning at startup, and a warning on every mock login. Never enable it in
/// production.
#[derive(Debug, Clone, Deserialize)]
pub struct MockOAuthConfig {
    /// Whether the mock provider is active. Defaults to false so merely having
    /// the section present (e.g. via a stray env var) is not sufficient.
    #[serde(default)]
    pub enabled: bool,
}

fn default_current_tos_version() -> i32 {
    1
}
fn default_github_auth_url() -> String {
    "https://github.com/login/oauth/authorize".to_string()
}
fn default_github_token_url() -> String {
    "https://github.com/login/oauth/access_token".to_string()
}
fn default_github_api_base_url() -> String {
    "https://api.github.com".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    /// IP address of database server, OR path to Unix socket.
    ///
    /// **NOTE**: if this is a path to a unix socket, `port` MUST be set to `None`.
    pub host: String,
    /// Port of the database server, or `None` if using a Unix socket.
    pub port: Option<u16>,
    /// Name of the database to connect to.
    pub database: String,
    /// Name of the user to connect with.
    pub user: String,
    /// Authentication credentials, if necessary.
    pub auth: Option<DatabaseCredentials>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseCredentials {
    /// Use a password to connect to the database.
    Password(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Socket address to bind to.
    pub bind_address: SocketAddr,
    /// Header names, in priority order, to trust for the real client address
    /// when the switchboard runs behind a reverse proxy (e.g.
    /// `["X-Forwarded-For"]`). Empty (the default) means trust ONLY the raw
    /// socket peer address: never read these headers unless a trusted proxy is
    /// known to set them, or a client could spoof its recorded address.
    #[serde(default)]
    pub trusted_proxy_headers: Vec<String>,
    /// Origins allowed to call the API cross-origin from a browser (e.g. a
    /// separately-hosted web console): exact `scheme://host[:port]` matches,
    /// or the single entry `"*"` to allow any origin. Empty (the default)
    /// emits no CORS headers at all, so browsers only permit same-origin use.
    ///
    /// The API is pure bearer-token (no cookies), so a wide-open `"*"` leaks
    /// no ambient credentials today; the allowlist exists as posture against
    /// anything cookie- or session-shaped appearing later. An origin listed
    /// here is NOT thereby a valid login `return_to` target — that is
    /// [`OAuthConfig::return_to_allowlist`], which matches full URLs.
    #[serde(default)]
    pub cors_allowed_origins: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceConfig {
    /// The Terms of Service version currently in force. A user whose
    /// `users.tos_accepted_version` is below this (or NULL) must (re-)accept the
    /// ToS before a token is issued; a newly-registering user accepts at this
    /// version. Bump it to force everyone through the interstitial again. The
    /// served ToS text is [`crate::routes::auth::TOS_TEXT`].
    #[serde(default = "default_current_tos_version")]
    pub current_tos_version: i32,
    /// Default lifetime of a user session token.
    #[serde(with = "human_duration")]
    pub default_token_timeout: chrono::TimeDelta,
    /// Default per-job timeout.
    #[serde(with = "human_duration")]
    pub default_job_timeout: chrono::TimeDelta,
    /// Default time a job can be queued before it may be culled.
    #[serde(with = "human_duration")]
    pub default_queue_timeout: chrono::TimeDelta,
    /// Default interval between job-supervisor matching passes
    #[serde(with = "human_duration")]
    pub match_interval: chrono::TimeDelta,
    /// How recently a host's worker must have heartbeat (`hosts.last_seen_at`)
    /// for the scheduler to consider the host live and dispatch onto it. Should
    /// be a small multiple of `supervisor_ping_interval` so a single missed tick
    /// doesn't take a host out of rotation.
    #[serde(with = "human_duration")]
    pub host_liveness_timeout: chrono::TimeDelta,
    /// How often the switchboard should send PING messages to supervisor.
    #[serde(with = "humantime_serde")]
    pub supervisor_ping_interval: Duration,
    /// Time without a PONG response from a supervisor before the switchboard
    /// closes the connection.
    #[serde(with = "humantime_serde")]
    pub supervisor_pong_dead: Duration,
    /// How often the per-host worker runs a reconcile pass, converging the DB's
    /// desired state against the supervisor's reported state (and dispatching
    /// newly-scheduled jobs). Kept separate from `supervisor_ping_interval`: it
    /// sets scheduling latency, not liveness-detection cadence.
    #[serde(with = "humantime_serde")]
    pub supervisor_reconcile_interval: Duration,
}

/// Load the switchboard configuration.
pub fn load_configuration(path: Option<&Path>) -> anyhow::Result<SwitchboardConfig> {
    use figment::providers::{self, Format};
    let f = figment::Figment::new();

    let f = if let Some(p) = path {
        if !p.exists() {
            return Err(anyhow::anyhow!(
                "Specified configuration file '{}' does not exist",
                p.display()
            ));
        }

        f.merge(providers::Toml::file(p))
    } else {
        tracing::info!("No configuration file specified");
        f
    };

    let config: SwitchboardConfig = f
        .merge(providers::Env::prefixed("TML_").split("__"))
        .extract()
        .context("Failed to extract switchboard configuration")?;

    // Reject unusable CORS origins at startup rather than when the router
    // builds the header allowlist.
    for origin in &config.server.cors_allowed_origins {
        if origin != "*" {
            origin
                .parse::<http::HeaderValue>()
                .ok()
                .filter(|_| origin.parse::<http::Uri>().is_ok())
                .with_context(|| {
                    format!("invalid entry in server.cors_allowed_origins: {origin:?}")
                })?;
        }
    }

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> SwitchboardConfig {
        SwitchboardConfig {
            database: DatabaseConfig {
                host: "unused".to_string(),
                port: None,
                database: "unused".to_string(),
                user: "unused".to_string(),
                auth: None,
            },
            server: ServerConfig {
                bind_address: "127.0.0.1:8081".parse().unwrap(),
                trusted_proxy_headers: Vec::new(),
                cors_allowed_origins: Vec::new(),
            },
            service: ServiceConfig {
                current_tos_version: 1,
                default_token_timeout: chrono::TimeDelta::hours(1),
                default_job_timeout: chrono::TimeDelta::hours(1),
                default_queue_timeout: chrono::TimeDelta::hours(1),
                match_interval: chrono::TimeDelta::seconds(1),
                host_liveness_timeout: chrono::TimeDelta::seconds(30),
                supervisor_ping_interval: Duration::from_secs(30),
                supervisor_pong_dead: Duration::from_secs(60),
                supervisor_reconcile_interval: Duration::from_secs(30),
            },
            oauth: OAuthConfig::default(),
            log_streaming: None,
        }
    }

    #[test]
    fn return_to_matches_allowlist_exactly() {
        let mut cfg = config();
        cfg.oauth.return_to_allowlist = vec!["https://console.example/auth/landing".to_string()];

        assert!(cfg.return_to_allowed("https://console.example/auth/landing"));
        // Exact match only: no prefix, origin, sub-path, or scheme laxness.
        assert!(!cfg.return_to_allowed("https://console.example/auth/landing/x"));
        assert!(!cfg.return_to_allowed("https://console.example/"));
        assert!(!cfg.return_to_allowed("http://console.example/auth/landing"));
        assert!(!cfg.return_to_allowed("https://console.example.evil/auth/landing"));
        assert!(!cfg.return_to_allowed(""));
    }

    #[test]
    fn empty_allowlist_allows_nothing() {
        let cfg = config();
        assert!(!cfg.return_to_allowed("https://console.example/auth/landing"));
    }
}

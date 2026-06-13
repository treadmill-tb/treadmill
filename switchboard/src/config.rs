use miette::{IntoDiagnostic, WrapErr};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
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
    /// Configuration of Switchboard logging.
    pub log: LogConfig,
    /// OAuth login providers. Optional: a deployment without any provider
    /// configured simply cannot issue interactive logins.
    #[serde(default)]
    pub oauth: OAuthConfig,
}

/// OAuth login provider configuration. Each provider is independently optional.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OAuthConfig {
    /// GitHub login, if configured.
    pub github: Option<GitHubOAuthConfig>,
    /// Where to send a browser after a successful interactive login.
    ///
    /// The callback is built for programmatic clients: it returns the session
    /// token as a JSON body. A browser frontend (the console) cannot consume
    /// that without JS, so when this is set the callback instead `302`-redirects
    /// the browser here with `?token=…&expires_at=…`; the frontend stores the
    /// token and strips it from the URL. When unset, the callback returns JSON
    /// as before. This is provider-independent (it is about the console, not the
    /// provider). See `TODOS.md` for the planned hardening (the token currently
    /// transits a URL query string).
    #[serde(default)]
    pub browser_success_redirect: Option<String>,
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
    /// OAuth scopes to request. Defaults cover profile, verified emails, and org
    /// membership (the latter feeds GitHub-org auto-groups).
    #[serde(default = "default_github_scopes")]
    pub scopes: Vec<String>,
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
fn default_github_scopes() -> Vec<String> {
    vec![
        "read:user".to_string(),
        "user:email".to_string(),
        "read:org".to_string(),
    ]
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
    /// Optional TLS mode for testing only.
    pub testing_only_tls_config: Option<TestingOnlyTlsConfig>,
    /// Header names, in priority order, to trust for the real client address
    /// when the switchboard runs behind a reverse proxy (e.g.
    /// `["X-Forwarded-For"]`). Empty (the default) means trust ONLY the raw
    /// socket peer address: never read these headers unless a trusted proxy is
    /// known to set them, or a client could spoof its recorded address.
    #[serde(default)]
    pub trusted_proxy_headers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TestingOnlyTlsConfig {
    /// Public key (for TLS).
    pub cert: PathBuf,
    /// Private key (for TLS).
    pub key: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceConfig {
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

#[derive(Debug, Clone, Deserialize)]
pub struct LogConfig {
    /// Whether to include `console-subscriber`.
    pub use_tokio_console_subscriber: bool,
}

/// Load the switchboard configuration.
pub fn load_configuration(path: Option<&Path>) -> miette::Result<SwitchboardConfig> {
    use figment::providers::{self, Format};
    let f = figment::Figment::new();

    let f = if let Some(p) = path {
        if !p.exists() {
            return Err(miette::miette!(
                "Specified configuration file '{}' does not exist",
                p.display()
            ));
        }

        f.merge(providers::Toml::file(p))
    } else {
        tracing::info!("No configuration file specified");
        f
    };

    f.merge(providers::Env::prefixed("TML_").split("__"))
        .extract()
        .into_diagnostic()
        .wrap_err("Failed to extract switchboard configuration")
}

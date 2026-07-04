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
    /// Optionally serve the web console from this same process/port. Absent (the
    /// default) means the console is not embedded; run it as its own service.
    #[serde(default)]
    pub console: Option<EmbeddedConsoleConfig>,
    /// Log streaming via NATS/JetStream. Absent (the default) disables the
    /// feature: jobs dispatch without a log-streaming destination and the
    /// read-token API is unavailable.
    #[serde(default)]
    pub log_streaming: Option<LogStreamingConfig>,
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

/// Configuration for serving the web console embedded in the switchboard.
///
/// When enabled, the console's routes are mounted at `/` on the switchboard's
/// own listener, alongside the API at `/api/v1` — so a single port serves both.
/// The console is itself an HTTP client of the switchboard API; embedded, it
/// calls back over loopback by default.
#[derive(Debug, Clone, Deserialize)]
pub struct EmbeddedConsoleConfig {
    /// Whether to mount the console. Off unless `true`, so merely having the
    /// section present is not sufficient.
    #[serde(default)]
    pub enabled: bool,
    /// External origin the console is reached at (e.g. `https://tml.example`).
    /// Only its scheme matters here: an `https://` value makes the session
    /// cookie `Secure`. Defaults to `http://127.0.0.1:<server port>`.
    #[serde(default)]
    pub public_base_url: Option<String>,
    /// Base URL the embedded console uses to call the switchboard API. Defaults
    /// to a loopback URL for this process (`http://127.0.0.1:<server port>`),
    /// which is correct unless TLS or a non-loopback bind requires otherwise.
    #[serde(default)]
    pub api_base_url: Option<String>,
}

/// OAuth login provider configuration. Each provider is independently optional.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OAuthConfig {
    /// GitHub login, if configured.
    pub github: Option<GitHubOAuthConfig>,
    /// Development-only mock login, if configured. See [`MockOAuthConfig`].
    pub mock: Option<MockOAuthConfig>,
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
    /// Where to send a browser that must accept the Terms of Service before its
    /// login completes (the ToS interstitial). Sibling of
    /// [`browser_success_redirect`](Self::browser_success_redirect): when set, a
    /// login that has passed admission but still needs consent (a brand-new user,
    /// or an existing user whose accepted ToS version is below the current one)
    /// is `302`-redirected here with `?pending_id=…&tos_version=…` appended,
    /// instead of receiving the `409` JSON marker. The frontend renders the ToS
    /// (see `GET /auth/tos`) and, on acceptance, `POST`s the pending id back to
    /// `/auth/tos/accept` (JSON or a plain HTML form) to finish the login. When
    /// unset, the callback returns the `409` `tos_required` JSON marker for
    /// programmatic clients.
    #[serde(default)]
    pub browser_tos_redirect: Option<String>,
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
    /// Header names, in priority order, to trust for the real client address
    /// when the switchboard runs behind a reverse proxy (e.g.
    /// `["X-Forwarded-For"]`). Empty (the default) means trust ONLY the raw
    /// socket peer address: never read these headers unless a trusted proxy is
    /// known to set them, or a client could spoof its recorded address.
    #[serde(default)]
    pub trusted_proxy_headers: Vec<String>,
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

    f.merge(providers::Env::prefixed("TML_").split("__"))
        .extract()
        .context("Failed to extract switchboard configuration")
}

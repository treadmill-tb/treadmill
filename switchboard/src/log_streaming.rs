//! Per-job log-streaming dispatch: minting NATS tokens and provisioning the
//! per-job JetStream streams.
//!
//! The switchboard holds a NATS account signing seed
//! ([`LogStreamingConfig::account_seed`]; the account identity seed doubles as
//! the signing seed) and mints per-job **bearer** user JWTs from it: each token
//! gets a fresh ephemeral user nkey whose seed is discarded after signing, so
//! the holder authenticates with the JWT string alone and the NATS server
//! itself enforces the granted subject scope. Every token is deny-by-default:
//! a direction (publish/subscribe) without an explicit grant is closed, never
//! left unrestricted (see [`mint_token`]).
//!
//! The opaque API token in [`crate::auth::token`] is unrelated to these JWTs.
//!
//! Token minting is pure and unit-tested; stream creation needs a live NATS
//! server, so it sits behind [`LogStreamProvisioner`] and is exercised by
//! hermetic tests gated on `TML_TEST_NATS_SERVER`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nats_jwt::{KeyPair, Token};
use uuid::Uuid;

use treadmill_rs::api::switchboard_supervisor::LogStreamingDispatch;

use crate::config::LogStreamingConfig;

/// The NATS subject prefix for a job's logs: `logs.<job-id>`. A channel token
/// is appended as `<prefix>.<channel>` (see
/// [`treadmill_rs::api::switchboard_supervisor::LogChannel`]).
pub fn subject_prefix(job_id: Uuid) -> String {
    format!("logs.{job_id}")
}

/// The subject wildcard covering every channel of a job: `logs.<job-id>.>`.
pub fn subject_scope(job_id: Uuid) -> String {
    format!("logs.{job_id}.>")
}

/// JetStream stream name for a job's logs: `logs-<job-id>` (stream names may
/// not contain `.`, so the dotted subject prefix cannot be reused).
pub fn stream_name(job_id: Uuid) -> String {
    format!("logs-{job_id}")
}

/// The NATS subject carrying user-typed console input for a job:
/// `console-in.<job-id>`. Deliberately outside the `logs.<job-id>.>` hierarchy:
/// typed input may contain secrets and must stay out of read-token holders'
/// reach.
pub fn console_input_subject(job_id: Uuid) -> String {
    format!("console-in.{job_id}")
}

/// JetStream stream name recording a job's console input:
/// `console-in-<job-id>`. Bound to [`console_input_subject`], so the server
/// captures every publish regardless of client behavior; nothing consumes it —
/// it is an audit record.
pub fn console_input_stream_name(job_id: Uuid) -> String {
    format!("console-in-{job_id}")
}

/// The inbox prefix a read client must use for its request/reply inboxes:
/// `_INBOX.logs-<job-id>`. Confining each token to a per-job prefix keeps
/// other users' JetStream API replies — which carry other jobs' log data — out
/// of reach; the account-wide `_INBOX.>` is never granted.
pub fn inbox_prefix(job_id: Uuid) -> String {
    format!("_INBOX.logs-{job_id}")
}

/// The inbox prefix the supervisor must use for its request/reply inboxes
/// (JetStream publish acks): `_INBOX.sup-<job-id>`. Distinct from the read
/// clients' [`inbox_prefix`] so the two roles' inbox spaces stay disjoint.
pub fn supervisor_inbox_prefix(job_id: Uuid) -> String {
    format!("_INBOX.sup-{job_id}")
}

/// The JetStream API subject prefix: `$JS.API`, or `$JS.<domain>.API` when the
/// server is configured with a JetStream domain.
fn jetstream_api_prefix(config: &LogStreamingConfig) -> String {
    match &config.jetstream_domain {
        Some(domain) => format!("$JS.{domain}.API"),
        None => "$JS.API".to_string(),
    }
}

/// What a minted token authorizes on a job's subjects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenScope {
    /// The supervisor: publish this job's console output and receive its
    /// typed console input.
    Supervisor,
    /// A client tailing and replaying this job's logs.
    Read,
    /// A client sending typed console input.
    ConsoleInput,
}

/// The subject grants of one token. A direction left empty is explicitly
/// denied when the token is minted, never left open.
struct Grants {
    publish: Vec<String>,
    subscribe: Vec<String>,
}

impl TokenScope {
    /// Short label used in the token's friendly name (diagnostic only).
    fn label(self) -> &'static str {
        match self {
            TokenScope::Supervisor => "sup",
            TokenScope::Read => "read",
            TokenScope::ConsoleInput => "input",
        }
    }

    fn grants(self, config: &LogStreamingConfig, job_id: Uuid) -> Grants {
        match self {
            TokenScope::Supervisor => Grants {
                publish: vec![subject_scope(job_id)],
                // JetStream publish acks arrive on request/reply inboxes, so
                // the supervisor needs its per-job inbox prefix besides the
                // console-input subject.
                subscribe: vec![
                    console_input_subject(job_id),
                    format!("{}.>", supervisor_inbox_prefix(job_id)),
                ],
            },
            // No subscribe at all, and no JetStream API: the input stream is
            // bound to the subject, so the server records every publish
            // itself.
            TokenScope::ConsoleInput => Grants {
                publish: vec![console_input_subject(job_id)],
                subscribe: vec![],
            },
            // The job-scoped JetStream API slice an ordered consumer needs
            // for bounded replay-then-follow: STREAM.INFO to compute the
            // replay start, CONSUMER.CREATE in both its legacy and named
            // forms, and INFO / MSG.NEXT (ordered consumers are pull-based) /
            // DELETE for the created consumer.
            TokenScope::Read => {
                let api = jetstream_api_prefix(config);
                let stream = stream_name(job_id);
                Grants {
                    publish: vec![
                        format!("{api}.STREAM.INFO.{stream}"),
                        format!("{api}.CONSUMER.CREATE.{stream}"),
                        format!("{api}.CONSUMER.CREATE.{stream}.>"),
                        format!("{api}.CONSUMER.INFO.{stream}.>"),
                        format!("{api}.CONSUMER.MSG.NEXT.{stream}.>"),
                        format!("{api}.CONSUMER.DELETE.{stream}.>"),
                    ],
                    subscribe: vec![subject_scope(job_id), format!("{}.>", inbox_prefix(job_id))],
                }
            }
        }
    }
}

/// Failure to mint a per-job user JWT.
#[derive(Debug, thiserror::Error)]
pub enum MintError {
    /// The configured account signing seed is not a valid nkey account seed.
    #[error("invalid NATS account signing seed: {0}")]
    Seed(String),
}

/// Mint a per-job **bearer** user JWT with the scope's [`Grants`], signed with
/// the account signing seed. `expires_in`, when set, bounds the token's
/// lifetime; the supervisor write token is minted without expiry and re-minted
/// on every dispatch, while client tokens are short-lived.
pub fn mint_token(
    config: &LogStreamingConfig,
    job_id: Uuid,
    scope: TokenScope,
    expires_in: Option<Duration>,
) -> Result<String, MintError> {
    // The account seed doubles as the signing key, so the JWT's issuer is the
    // account's own public key.
    let account =
        KeyPair::from_seed(&config.account_seed).map_err(|e| MintError::Seed(e.to_string()))?;

    // Ephemeral subject identity, discarded after signing: a bearer token
    // carries no nkey challenge, so the seed is never needed again.
    let user = KeyPair::new_user();

    let mut token = Token::new_user(account.public_key(), user.public_key())
        .name(format!("tml-job-{job_id}-{}", scope.label()))
        .bearer_token(true);

    // Deny-by-default: NATS treats a direction carrying no permissions at all
    // as *unrestricted*, so a direction without grants must be explicitly
    // closed. (A blanket deny must never be combined with grants: deny
    // overrides allow on overlapping subjects.)
    let Grants { publish, subscribe } = scope.grants(config, job_id);
    token = if publish.is_empty() {
        token.deny_publish(">")
    } else {
        publish.into_iter().fold(token, |t, s| t.allow_publish(s))
    };
    token = if subscribe.is_empty() {
        token.deny_subscribe(">")
    } else {
        subscribe
            .into_iter()
            .fold(token, |t, s| t.allow_subscribe(s))
    };

    if let Some(ttl) = expires_in {
        // Saturating: an absurd TTL must not wrap into a past `exp`.
        let exp = chrono::Utc::now()
            .timestamp()
            .saturating_add(ttl.as_secs() as i64);
        token = token.expires(exp);
    }

    Ok(token.sign(&account))
}

/// Build the [`LogStreamingDispatch`] handed to a supervisor in
/// `StartJobMessage`. The write token carries no expiry — it is re-minted on
/// every (re)dispatch rather than persisted.
pub fn build_dispatch(
    config: &LogStreamingConfig,
    job_id: Uuid,
) -> Result<LogStreamingDispatch, MintError> {
    let write_token = mint_token(config, job_id, TokenScope::Supervisor, None)?;
    Ok(LogStreamingDispatch {
        nats_url: config.nats_url.clone(),
        subject_prefix: subject_prefix(job_id),
        write_token,
        console_input_subject: Some(console_input_subject(job_id)),
        inbox_prefix: Some(supervisor_inbox_prefix(job_id)),
    })
}

/// Failure to provision a job's JetStream stream.
#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    /// The JetStream request to create (or look up) the stream failed.
    #[error("provisioning JetStream stream for job {job_id}: {source}")]
    Stream {
        job_id: Uuid,
        #[source]
        source: anyhow::Error,
    },
}

/// Creates the per-job JetStream streams. Behind a trait so the dispatch path
/// can be tested without a live NATS server; production uses
/// [`NatsLogStreamProvisioner`].
#[async_trait]
pub trait LogStreamProvisioner: Send + Sync {
    /// Idempotently create the per-job stream capturing `logs.<job-id>.>`.
    /// Must be safe to call repeatedly: reconcile re-dispatches `StartJob`
    /// idempotently, so an already-existing stream is a success.
    async fn ensure_job_stream(&self, job_id: Uuid) -> Result<(), ProvisionError>;

    /// Idempotently create the per-job stream recording `console-in.<job-id>`.
    /// Called **before** a console-input token is minted, so capture is in
    /// place before any client can publish.
    async fn ensure_console_input_stream(&self, job_id: Uuid) -> Result<(), ProvisionError>;
}

/// The log-streaming components the switchboard wires through to the dispatch
/// path. Cloneable; a single instance is built at startup and shared by all
/// supervisor workers.
#[derive(Clone)]
pub struct LogStreaming {
    /// Config for token minting and the dispatch payload's `nats_url`.
    pub config: LogStreamingConfig,
    /// Shared stream provisioner.
    pub provisioner: Arc<dyn LogStreamProvisioner>,
}

/// Failure to establish the switchboard's management connection to NATS.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The configured account signing seed is not a valid nkey account seed.
    #[error("invalid NATS account signing seed: {0}")]
    Seed(String),
    /// The connection to the NATS server could not be established.
    #[error("connecting to NATS at {url}: {source}")]
    Connect {
        url: String,
        #[source]
        source: async_nats::ConnectError,
    },
}

/// The inbox prefix of the switchboard's own management connection.
const MGMT_INBOX_PREFIX: &str = "_INBOX.tml-switchboard-mgmt";

/// The production [`LogStreamProvisioner`]: creates streams over a JetStream
/// management connection to a real NATS server.
pub struct NatsLogStreamProvisioner {
    jetstream: async_nats::jetstream::Context,
}

impl NatsLogStreamProvisioner {
    /// Connect to NATS and build a provisioner with JetStream management
    /// rights. The management user's JWT is **non-bearer** — the switchboard
    /// holds its nkey seed and answers the server's nonce challenge — and is
    /// scoped to the JetStream API plus the management inbox prefix.
    pub async fn connect(config: &LogStreamingConfig) -> Result<Self, ConnectError> {
        let account = KeyPair::from_seed(&config.account_seed)
            .map_err(|e| ConnectError::Seed(e.to_string()))?;

        let mgmt_user = Arc::new(KeyPair::new_user());
        let mgmt_jwt = Token::new_user(account.public_key(), mgmt_user.public_key())
            .name("tml-switchboard-logstream-mgmt")
            .allow_publish(format!("{}.>", jetstream_api_prefix(config)))
            .allow_subscribe(format!("{MGMT_INBOX_PREFIX}.>"))
            .sign(&account);

        let signer = mgmt_user.clone();
        let client = async_nats::ConnectOptions::with_jwt(mgmt_jwt, move |nonce| {
            let signer = signer.clone();
            async move { signer.sign(&nonce).map_err(async_nats::AuthError::new) }
        })
        .name("tml-switchboard")
        .custom_inbox_prefix(MGMT_INBOX_PREFIX)
        .connect(&config.nats_url)
        .await
        .map_err(|source| ConnectError::Connect {
            url: config.nats_url.clone(),
            source,
        })?;

        let jetstream = match &config.jetstream_domain {
            Some(domain) => async_nats::jetstream::with_domain(client, domain),
            None => async_nats::jetstream::new(client),
        };

        Ok(Self { jetstream })
    }

    /// Idempotently create a stream capturing `subjects` under `name`;
    /// `get_or_create_stream` returns an existing same-named stream, so a
    /// repeat call is a no-op.
    async fn ensure_stream(
        &self,
        job_id: Uuid,
        name: String,
        subjects: Vec<String>,
    ) -> Result<(), ProvisionError> {
        let stream_config = async_nats::jetstream::stream::Config {
            name,
            subjects,
            // Zero means "unlimited age": the stream never expires (GC is a
            // separate concern).
            max_age: Duration::ZERO,
            ..Default::default()
        };

        self.jetstream
            .get_or_create_stream(stream_config)
            .await
            .map_err(|source| ProvisionError::Stream {
                job_id,
                source: source.into(),
            })?;

        Ok(())
    }
}

#[async_trait]
impl LogStreamProvisioner for NatsLogStreamProvisioner {
    async fn ensure_job_stream(&self, job_id: Uuid) -> Result<(), ProvisionError> {
        self.ensure_stream(job_id, stream_name(job_id), vec![subject_scope(job_id)])
            .await
    }

    async fn ensure_console_input_stream(&self, job_id: Uuid) -> Result<(), ProvisionError> {
        self.ensure_stream(
            job_id,
            console_input_stream_name(job_id),
            vec![console_input_subject(job_id)],
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use serde_json::Value;

    /// A throwaway account seed per run keeps a real secret out of the tree.
    fn test_config() -> LogStreamingConfig {
        let account = KeyPair::new_account();
        LogStreamingConfig {
            nats_url: "nats://nats.example:4222".to_string(),
            websocket_url: None,
            jetstream_domain: None,
            account_seed: account.seed().expect("account seed"),
        }
    }

    /// Decode the (unverified) claims out of a JWT's payload segment as JSON.
    /// Parsed as a [`Value`] rather than `nats_jwt`'s `Claims` (which cannot
    /// round-trip its own output); this also asserts the exact wire shape the
    /// NATS server will see.
    fn decode_claims(jwt: &str) -> Value {
        let payload = jwt.split('.').nth(1).expect("jwt has a payload segment");
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .expect("payload is base64url");
        serde_json::from_slice(&bytes).expect("payload is JSON")
    }

    /// A permission list (`"allow"` / `"deny"`) under a direction (`"pub"` /
    /// `"sub"`) of a user JWT's NATS claims (empty if absent).
    fn permission_list(claims: &Value, direction: &str, list: &str) -> Vec<String> {
        claims["nats"][direction][list]
            .as_array()
            .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
            .unwrap_or_default()
    }

    fn allow(claims: &Value, direction: &str) -> Vec<String> {
        permission_list(claims, direction, "allow")
    }

    fn deny(claims: &Value, direction: &str) -> Vec<String> {
        permission_list(claims, direction, "deny")
    }

    #[test]
    fn subject_helpers_are_consistent() {
        let job_id = Uuid::nil();
        assert_eq!(
            subject_prefix(job_id),
            "logs.00000000-0000-0000-0000-000000000000"
        );
        assert_eq!(
            subject_scope(job_id),
            "logs.00000000-0000-0000-0000-000000000000.>"
        );
        // Stream names forbid '.', so the prefixes are hyphenated.
        assert_eq!(
            stream_name(job_id),
            "logs-00000000-0000-0000-0000-000000000000"
        );
        assert_eq!(
            console_input_subject(job_id),
            "console-in.00000000-0000-0000-0000-000000000000"
        );
        assert_eq!(
            console_input_stream_name(job_id),
            "console-in-00000000-0000-0000-0000-000000000000"
        );
        assert_eq!(
            inbox_prefix(job_id),
            "_INBOX.logs-00000000-0000-0000-0000-000000000000"
        );
        assert_eq!(
            supervisor_inbox_prefix(job_id),
            "_INBOX.sup-00000000-0000-0000-0000-000000000000"
        );
    }

    #[test]
    fn write_token_is_bearer_and_scoped_to_the_job() {
        let config = test_config();
        let job_id = Uuid::new_v4();

        let jwt = mint_token(&config, job_id, TokenScope::Supervisor, None).expect("mint");
        let claims = decode_claims(&jwt);

        // Issued by the account's own public key (seed doubles as signing
        // key), which is also the issuer_account.
        let account_pub = KeyPair::from_seed(&config.account_seed)
            .unwrap()
            .public_key();
        assert_eq!(claims["iss"], account_pub);
        assert_eq!(claims["nats"]["type"], "user");
        assert_eq!(claims["nats"]["issuer_account"], account_pub);
        assert_eq!(claims["nats"]["bearer_token"], true);

        // Publish exactly this job's log subjects; subscribe exactly its
        // console-input subject and the per-job ack inbox prefix.
        assert_eq!(allow(&claims, "pub"), vec![subject_scope(job_id)]);
        assert_eq!(
            allow(&claims, "sub"),
            vec![
                console_input_subject(job_id),
                format!("{}.>", supervisor_inbox_prefix(job_id)),
            ]
        );

        // No expiry on the (re-minted-per-dispatch) write token.
        assert!(claims.get("exp").is_none());
    }

    #[test]
    fn console_input_token_may_only_publish_typed_input() {
        let config = test_config();
        let job_id = Uuid::new_v4();

        let jwt = mint_token(
            &config,
            job_id,
            TokenScope::ConsoleInput,
            Some(Duration::from_secs(300)),
        )
        .expect("mint");
        let claims = decode_claims(&jwt);

        assert_eq!(claims["nats"]["bearer_token"], true);

        // Publish exactly the job's console-input subject; subscribe nothing,
        // explicitly.
        assert_eq!(allow(&claims, "pub"), vec![console_input_subject(job_id)]);
        assert!(allow(&claims, "sub").is_empty());
        assert_eq!(deny(&claims, "sub"), vec![">"]);

        assert!(
            claims.get("exp").is_some(),
            "console input token must expire"
        );
    }

    #[test]
    fn read_token_is_job_scoped_and_expiring() {
        let config = test_config();
        let job_id = Uuid::new_v4();

        let jwt = mint_token(
            &config,
            job_id,
            TokenScope::Read,
            Some(Duration::from_secs(300)),
        )
        .expect("mint");
        let claims = decode_claims(&jwt);

        assert_eq!(claims["nats"]["bearer_token"], true);

        // Subscribe: the job's log subjects plus the job's own inbox prefix —
        // never the account-wide `_INBOX.>`.
        assert_eq!(
            allow(&claims, "sub"),
            vec![subject_scope(job_id), format!("{}.>", inbox_prefix(job_id))]
        );

        // Publish: exactly the job-scoped JetStream API slice an ordered
        // consumer needs, rooted in this job's stream name.
        let stream = stream_name(job_id);
        assert_eq!(
            allow(&claims, "pub"),
            vec![
                format!("$JS.API.STREAM.INFO.{stream}"),
                format!("$JS.API.CONSUMER.CREATE.{stream}"),
                format!("$JS.API.CONSUMER.CREATE.{stream}.>"),
                format!("$JS.API.CONSUMER.INFO.{stream}.>"),
                format!("$JS.API.CONSUMER.MSG.NEXT.{stream}.>"),
                format!("$JS.API.CONSUMER.DELETE.{stream}.>"),
            ]
        );

        assert!(claims.get("exp").is_some(), "read token must expire");
    }

    /// Deny-by-default: no scope may mint a token with an unrestricted
    /// direction — each direction either has explicit grants or denies `>`.
    #[test]
    fn no_scope_leaves_a_direction_unrestricted() {
        let config = test_config();
        let job_id = Uuid::new_v4();

        for scope in [
            TokenScope::Supervisor,
            TokenScope::Read,
            TokenScope::ConsoleInput,
        ] {
            let claims = decode_claims(&mint_token(&config, job_id, scope, None).expect("mint"));
            for direction in ["pub", "sub"] {
                let granted = !allow(&claims, direction).is_empty();
                let closed = deny(&claims, direction) == vec![">".to_string()];
                assert!(
                    granted ^ closed,
                    "{scope:?} must grant or close '{direction}', never leave it open"
                );
            }
        }
    }

    #[test]
    fn read_token_addresses_the_configured_jetstream_domain() {
        let config = LogStreamingConfig {
            jetstream_domain: Some("hub".to_string()),
            ..test_config()
        };
        let job_id = Uuid::new_v4();

        let jwt = mint_token(&config, job_id, TokenScope::Read, None).expect("mint");
        let claims = decode_claims(&jwt);

        // With a domain, every JetStream API grant goes through
        // `$JS.<domain>.API` instead of `$JS.API`.
        let pub_allow = allow(&claims, "pub");
        assert!(!pub_allow.is_empty());
        for subject in &pub_allow {
            assert!(
                subject.starts_with("$JS.hub.API."),
                "expected domain-prefixed API subject, got {subject}"
            );
        }
    }

    #[test]
    fn distinct_jobs_get_distinct_subjects() {
        let config = test_config();
        let a = mint_token(&config, Uuid::new_v4(), TokenScope::Supervisor, None).unwrap();
        let b = mint_token(&config, Uuid::new_v4(), TokenScope::Supervisor, None).unwrap();
        assert_ne!(
            allow(&decode_claims(&a), "pub"),
            allow(&decode_claims(&b), "pub"),
            "each job's token scopes only its own subjects"
        );
    }

    #[test]
    fn build_dispatch_carries_url_subjects_and_a_write_token() {
        let config = test_config();
        let job_id = Uuid::new_v4();

        let dispatch = build_dispatch(&config, job_id).expect("dispatch");
        assert_eq!(dispatch.nats_url, config.nats_url);
        assert_eq!(dispatch.subject_prefix, subject_prefix(job_id));
        assert_eq!(
            dispatch.console_input_subject,
            Some(console_input_subject(job_id))
        );
        assert_eq!(dispatch.inbox_prefix, Some(supervisor_inbox_prefix(job_id)));
        let claims = decode_claims(&dispatch.write_token);
        assert_eq!(allow(&claims, "pub"), vec![subject_scope(job_id)]);
        assert_eq!(
            allow(&claims, "sub"),
            vec![
                console_input_subject(job_id),
                format!("{}.>", supervisor_inbox_prefix(job_id)),
            ]
        );
    }

    #[test]
    fn a_bad_seed_is_rejected() {
        let config = LogStreamingConfig {
            nats_url: "nats://nats.example:4222".to_string(),
            websocket_url: None,
            jetstream_domain: None,
            account_seed: "not-a-real-seed".to_string(),
        };
        let err = mint_token(&config, Uuid::new_v4(), TokenScope::Supervisor, None).unwrap_err();
        assert!(matches!(err, MintError::Seed(_)));
    }

    // ---- Live NATS tests (hermetic Nix check) -----------------------------
    //
    // Gated on `TML_TEST_NATS_SERVER` (the nats-server binary), set only by
    // the `nats-log-streaming` Nix check; unset → the test skips (the sandbox
    // cannot run a broker).

    fn nats_server_bin() -> Option<std::path::PathBuf> {
        std::env::var_os("TML_TEST_NATS_SERVER")
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_file())
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// A child `nats-server` with JetStream enabled; killed on drop, with its
    /// store directory removed.
    struct NatsServer {
        child: std::process::Child,
        port: u16,
        store: std::path::PathBuf,
    }

    impl Drop for NatsServer {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.store);
        }
    }

    impl NatsServer {
        fn url(&self) -> String {
            format!("nats://127.0.0.1:{}", self.port)
        }

        /// Start with no auth.
        async fn start(bin: &std::path::Path) -> Self {
            let store =
                std::env::temp_dir().join(format!("tml-switchboard-nats-{}", Uuid::new_v4()));
            let port = free_port();
            let child = std::process::Command::new(bin)
                .arg("-js")
                .arg("-sd")
                .arg(&store)
                .arg("-a")
                .arg("127.0.0.1")
                .arg("-p")
                .arg(port.to_string())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn nats-server");
            let server = NatsServer { child, port, store };
            for _ in 0..100 {
                if async_nats::connect(server.url()).await.is_ok() {
                    return server;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            panic!("nats-server did not become ready");
        }

        /// Start from a rendered config file (`render(port, store_dir)`), for
        /// servers with auth. Readiness is polled with `ready_connect` since
        /// anonymous connects are rejected once accounts are configured.
        async fn start_with_config(
            bin: &std::path::Path,
            render: impl FnOnce(u16, &str) -> String,
            ready_connect: async_nats::ConnectOptions,
        ) -> Self {
            let store =
                std::env::temp_dir().join(format!("tml-switchboard-nats-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&store).expect("create store dir");
            let port = free_port();
            let conf_path = store.join("server.conf");
            std::fs::write(&conf_path, render(port, store.to_str().unwrap()))
                .expect("write server config");
            let child = std::process::Command::new(bin)
                .arg("-c")
                .arg(&conf_path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn nats-server");
            let server = NatsServer { child, port, store };
            for _ in 0..100 {
                if ready_connect.clone().connect(server.url()).await.is_ok() {
                    return server;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            panic!("nats-server did not become ready");
        }
    }

    /// Render the config `permissions` clause enforcing a token's decoded
    /// allow/deny lists on a config-file user: password auth stands in for
    /// the bearer-JWT transport, while the permission set under test is
    /// exactly the minted one.
    fn permissions_clause(claims: &Value) -> String {
        let quote = |subjects: Vec<String>| {
            subjects
                .iter()
                .map(|s| format!("\"{s}\""))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let direction = |config_key: &str, direction: &str| {
            let mut lists = Vec::new();
            let allowed = allow(claims, direction);
            if !allowed.is_empty() {
                lists.push(format!("allow: [{}]", quote(allowed)));
            }
            let denied = deny(claims, direction);
            if !denied.is_empty() {
                lists.push(format!("deny: [{}]", quote(denied)));
            }
            format!("{config_key}: {{ {} }}", lists.join(", "))
        };
        format!(
            "permissions: {{ {}, {} }}",
            direction("publish", "pub"),
            direction("subscribe", "sub")
        )
    }

    /// A server config with JetStream, an unrestricted `mgmt` user, and a
    /// `restricted` user under the given permissions clause.
    fn restricted_user_config(permissions: &str) -> impl FnOnce(u16, &str) -> String + '_ {
        move |port, store| {
            format!(
                r#"
listen: "127.0.0.1:{port}"
jetstream {{ store_dir: "{store}" }}
accounts {{
  TEST {{
    jetstream: enabled
    users: [
      {{ user: "mgmt", password: "mgmt" }},
      {{ user: "restricted", password: "restricted", {permissions} }}
    ]
  }}
}}
"#
            )
        }
    }

    fn mgmt_opts() -> async_nats::ConnectOptions {
        async_nats::ConnectOptions::new().user_and_password("mgmt".to_string(), "mgmt".to_string())
    }

    fn restricted_opts() -> async_nats::ConnectOptions {
        async_nats::ConnectOptions::new()
            .user_and_password("restricted".to_string(), "restricted".to_string())
    }

    #[tokio::test]
    async fn nats_live_provisioner_creates_job_stream() {
        let Some(bin) = nats_server_bin() else {
            eprintln!("TML_TEST_NATS_SERVER unset; skipping live NATS provisioner test");
            return;
        };
        let server = NatsServer::start(&bin).await;

        // No-auth connection: this asserts the provisioning behavior, not the
        // (separately tested) auth model.
        let client = async_nats::connect(server.url()).await.unwrap();
        let jetstream = async_nats::jetstream::new(client);
        let provisioner = NatsLogStreamProvisioner { jetstream };

        let job_id = Uuid::new_v4();
        provisioner.ensure_job_stream(job_id).await.expect("create");
        // Idempotent: a re-dispatch must be a no-op, not an error.
        provisioner
            .ensure_job_stream(job_id)
            .await
            .expect("idempotent re-create");

        // The stream captures exactly this job's subjects and never expires.
        let stream = provisioner
            .jetstream
            .get_stream(stream_name(job_id))
            .await
            .expect("stream exists");
        let config = &stream.cached_info().config;
        assert_eq!(config.subjects, vec![subject_scope(job_id)]);
        assert_eq!(config.max_age, Duration::ZERO);

        // Same for the console-input recording stream.
        provisioner
            .ensure_console_input_stream(job_id)
            .await
            .expect("create input stream");
        provisioner
            .ensure_console_input_stream(job_id)
            .await
            .expect("idempotent input re-create");
        let stream = provisioner
            .jetstream
            .get_stream(console_input_stream_name(job_id))
            .await
            .expect("input stream exists");
        let config = &stream.cached_info().config;
        assert_eq!(config.subjects, vec![console_input_subject(job_id)]);
        assert_eq!(config.max_age, Duration::ZERO);
    }

    /// The supervisor scope must be *sufficient* for both directions of the
    /// serial channel: a durably **acked** JetStream publish (the ack arrives
    /// on the granted per-job inbox prefix) and receiving console input.
    #[tokio::test]
    async fn nats_live_supervisor_scope_suffices_for_acked_publish_and_input() {
        use futures_util::StreamExt as _;

        let Some(bin) = nats_server_bin() else {
            eprintln!("TML_TEST_NATS_SERVER unset; skipping live NATS supervisor-scope test");
            return;
        };

        let config = test_config();
        let job_id = Uuid::new_v4();
        let claims = decode_claims(
            &mint_token(&config, job_id, TokenScope::Supervisor, None).expect("mint"),
        );
        let permissions = permissions_clause(&claims);
        let server =
            NatsServer::start_with_config(&bin, restricted_user_config(&permissions), mgmt_opts())
                .await;

        let mgmt = mgmt_opts()
            .connect(server.url())
            .await
            .expect("mgmt connect");
        let provisioner = NatsLogStreamProvisioner {
            jetstream: async_nats::jetstream::new(mgmt.clone()),
        };
        provisioner.ensure_job_stream(job_id).await.expect("create");

        // Connect exactly as the supervisor does: the per-job inbox prefix is
        // the only subscribable reply space.
        let supervisor = restricted_opts()
            .custom_inbox_prefix(supervisor_inbox_prefix(job_id))
            .connect(server.url())
            .await
            .expect("supervisor connect");

        let supervisor_js = async_nats::jetstream::new(supervisor.clone());
        let ack = supervisor_js
            .publish(format!("{}.serial", subject_prefix(job_id)), "boot".into())
            .await
            .expect("publish")
            .await
            .expect("durable ack arrives on the granted inbox prefix");
        assert_eq!(ack.stream, stream_name(job_id));

        // Typed input published on the console-input subject reaches the
        // supervisor's subscription. The subscription registers
        // asynchronously and a core publish with no live subscription is
        // dropped, so republish until it arrives.
        let mut input = supervisor
            .subscribe(console_input_subject(job_id))
            .await
            .expect("subscribe");
        supervisor.flush().await.expect("flush subscription");
        let message = 'republish: {
            for _ in 0..100 {
                mgmt.publish(console_input_subject(job_id), "input".into())
                    .await
                    .expect("publish input");
                mgmt.flush().await.expect("flush publish");
                match tokio::time::timeout(Duration::from_millis(300), input.next()).await {
                    Ok(message) => break 'republish message.expect("subscription open"),
                    Err(_elapsed) => continue,
                }
            }
            panic!("console input never arrived on the granted subscription");
        };
        assert_eq!(message.payload.as_ref(), b"input");
    }

    /// The read scope must be *sufficient* for the web console's bounded
    /// replay-then-follow: STREAM.INFO for the start-seq computation, an
    /// ordered consumer over the job's stream, and the live tail — all through
    /// inboxes under the per-job prefix. The JS browser client drives the same
    /// `$JS.API` subjects as async-nats here.
    #[tokio::test]
    async fn nats_live_read_token_scope_suffices_for_bounded_replay() {
        let Some(bin) = nats_server_bin() else {
            eprintln!("TML_TEST_NATS_SERVER unset; skipping live NATS read-scope test");
            return;
        };

        let config = test_config();
        let job_id = Uuid::new_v4();

        let claims =
            decode_claims(&mint_token(&config, job_id, TokenScope::Read, None).expect("mint"));
        let permissions = permissions_clause(&claims);
        let server =
            NatsServer::start_with_config(&bin, restricted_user_config(&permissions), mgmt_opts())
                .await;

        // Management side: create the job's stream and store frames of a
        // known payload size, awaiting the durable ack for each.
        let mgmt = mgmt_opts()
            .connect(server.url())
            .await
            .expect("mgmt connect");
        let jetstream = async_nats::jetstream::new(mgmt);
        let provisioner = NatsLogStreamProvisioner {
            jetstream: jetstream.clone(),
        };
        provisioner.ensure_job_stream(job_id).await.expect("create");

        let subject = format!("{}.serial", subject_prefix(job_id));
        const STORED: u64 = 10;
        for i in 0..STORED {
            let payload = vec![b'0' + i as u8; 100];
            jetstream
                .publish(subject.clone(), payload.into())
                .await
                .expect("publish")
                .await
                .expect("ack");
        }

        // Read side, connecting exactly as the browser does: the restricted
        // user, inboxes under the per-job prefix only.
        let reader = restricted_opts()
            .custom_inbox_prefix(inbox_prefix(job_id))
            .connect(server.url())
            .await
            .expect("reader connect");
        let reader_js = async_nats::jetstream::new(reader);

        // STREAM.INFO, and the client's bounded-replay start computation: a
        // cap below the stored total must land the start past the first
        // message (truncated replay).
        let stream = reader_js
            .get_stream(stream_name(job_id))
            .await
            .expect("STREAM.INFO is granted");
        let state = &stream.cached_info().state;
        assert_eq!(state.messages, STORED);
        const CAP_BYTES: u64 = 350;
        assert!(state.bytes > CAP_BYTES, "test premise: stream exceeds cap");
        let avg = state.bytes / state.messages;
        let start = state
            .first_sequence
            .max(state.last_sequence - CAP_BYTES.div_ceil(avg) + 1);
        assert!(start > state.first_sequence, "replay must be truncated");

        // Ordered consumer from the computed start: CONSUMER.CREATE /
        // CONSUMER.INFO / CONSUMER.MSG.NEXT are granted.
        let consumer = stream
            .create_consumer(async_nats::jetstream::consumer::pull::OrderedConfig {
                deliver_policy: async_nats::jetstream::consumer::DeliverPolicy::ByStartSequence {
                    start_sequence: start,
                },
                ..Default::default()
            })
            .await
            .expect("ordered consumer creation is granted");
        let mut messages = consumer.messages().await.expect("consume");

        use futures_util::StreamExt as _;
        macro_rules! next {
            () => {
                tokio::time::timeout(Duration::from_secs(30), messages.next())
                    .await
                    .expect("message within timeout")
                    .expect("stream not exhausted")
                    .expect("message delivered")
            };
        }

        // The backlog replays exactly from the computed start...
        for seq in start..=state.last_sequence {
            let msg = next!();
            assert_eq!(msg.info().expect("info").stream_sequence, seq);
            assert_eq!(msg.payload[0], b'0' + (seq - 1) as u8);
        }

        // ...and the same consumer keeps following live publishes.
        jetstream
            .publish(subject.clone(), "live".into())
            .await
            .expect("publish")
            .await
            .expect("ack");
        let msg = next!();
        assert_eq!(msg.info().expect("info").stream_sequence, STORED + 1);
        assert_eq!(msg.payload.as_ref(), b"live");
    }
}

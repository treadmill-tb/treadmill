//! Per-job log-streaming dispatch: minting the supervisor's write token and
//! provisioning the per-job JetStream stream.
//!
//! Supervisor console output (qemu stdout/stderr, serial) is published to a
//! per-job JetStream stream under the subjects `logs.<job-id>.<channel>` (see
//! [`treadmill_rs::api::switchboard_supervisor::LogChannel`]). The switchboard
//! is the authority for both ends of that pipe:
//!
//! - it **mints** a per-job, publish-scoped **bearer** user JWT (the
//!   supervisor's `write_token`, handed over in `StartJobMessage`), and
//! - it **creates** the per-job stream at dispatch.
//!
//! ### Auth model
//!
//! The deployment runs a NATS decentralized-auth account whose **signing seed**
//! the switchboard holds as a secret ([`LogStreamingConfig::account_seed`]). The
//! account *identity* seed doubles as the *signing* seed, so a minted user JWT's
//! issuer is simply the account's own public key (derived from the seed) — there
//! is no separate signing key. Each minted token gets a fresh, ephemeral user
//! nkey as its subject; the token is a **bearer** token, so the holder connects
//! with the JWT string alone and the user nkey seed is discarded immediately.
//! The NATS server validates the granted pub/sub scope itself — that is the
//! "storage enforces the per-job token" property the design relies on.
//!
//! The opaque 32-byte API token in [`crate::auth::token`] is unrelated to these
//! NATS JWTs; do not conflate them.
//!
//! ### Testability
//!
//! Token minting and scope assembly are pure (no daemon, no I/O) and unit-tested
//! below. Stream creation needs a live NATS server, which cannot run in the
//! sandbox (`AGENTS.md` §2), so it is hidden behind the [`LogStreamProvisioner`]
//! trait: the production [`NatsLogStreamProvisioner`] talks JetStream, while the
//! dispatch path is exercised in `#[sqlx::test]`s with log streaming disabled
//! (no provisioner). The live round-trip belongs in a hermetic Nix check.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nats_jwt::{KeyPair, Token};
use uuid::Uuid;

use treadmill_rs::api::switchboard_supervisor::LogStreamingDispatch;

use crate::config::LogStreamingConfig;

/// The NATS subject prefix for a job's logs: `logs.<job-id>`. A channel token is
/// appended as `<prefix>.<channel>` (see
/// [`treadmill_rs::api::switchboard_supervisor::LogChannel`]). This is the value
/// carried in [`LogStreamingDispatch::subject_prefix`].
pub fn subject_prefix(job_id: Uuid) -> String {
    format!("logs.{job_id}")
}

/// The subject wildcard covering every channel of a job: `logs.<job-id>.>`. Used
/// both as the minted token's pub/sub scope and as the stream's captured
/// subject, and returned to read clients as the subject to subscribe to.
pub fn subject_scope(job_id: Uuid) -> String {
    format!("logs.{job_id}.>")
}

/// JetStream stream name for a job's logs.
///
/// Stream names may not contain spaces, tabs, or `.` characters, so the dotted
/// subject prefix cannot be reused verbatim; we use `logs-<job-id>` (the UUID's
/// hyphens are permitted). Returned to read clients so the naming convention
/// stays server-owned.
pub fn stream_name(job_id: Uuid) -> String {
    format!("logs-{job_id}")
}

/// The NATS subject carrying user-typed console input for a job:
/// `console-in.<job-id>` (exact, no channel suffix). Deliberately outside the
/// `logs.<job-id>.>` hierarchy: read-token holders may retrieve the job's log
/// stream, but typed input may contain secrets and must stay out of their
/// reach.
pub fn console_input_subject(job_id: Uuid) -> String {
    format!("console-in.{job_id}")
}

/// JetStream stream name recording a job's console input: `console-in-<job-id>`
/// (stream names may not contain `.`). Bound to [`console_input_subject`], so
/// the server captures every publish regardless of client behavior; nothing
/// consumes it — it is an audit record.
pub fn console_input_stream_name(job_id: Uuid) -> String {
    format!("console-in-{job_id}")
}

/// The inbox prefix a read client must use for its request/reply inboxes:
/// `_INBOX.logs-<job-id>`.
///
/// Read tokens may not subscribe to the account-wide `_INBOX.>` — other users'
/// JetStream API replies carry other jobs' log data — so each token is granted
/// only this per-job prefix, and the client sets it as its connection's inbox
/// prefix. Returned to read clients alongside the token.
pub fn inbox_prefix(job_id: Uuid) -> String {
    format!("_INBOX.logs-{job_id}")
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
    /// The supervisor's scope: publish this job's console output, and
    /// subscribe to the job's console-input subject to deliver typed input to
    /// the serial console.
    Supervisor,
    /// Read — a client tailing and replaying this job's logs: subscribe on the
    /// log subjects plus the job-scoped slice of the JetStream API needed to
    /// run an ordered consumer against the job's stream (replay-then-follow).
    Read,
    /// Console input — a client sending typed input: publish to the job's
    /// console-input subject, nothing else. No JetStream API access is needed:
    /// the input stream is bound to the subject, so the server records every
    /// publish itself.
    ConsoleInput,
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
}

/// Failure to mint a per-job user JWT.
#[derive(Debug, thiserror::Error)]
pub enum MintError {
    /// The configured account signing seed is not a valid nkey account seed.
    #[error("invalid NATS account signing seed: {0}")]
    Seed(String),
}

/// Mint a per-job **bearer** user JWT scoped to a single job's log subjects
/// (`logs.<job-id>.>`), signed with the account signing seed.
///
/// A fresh ephemeral user nkey is generated as the token's subject and then
/// discarded: bearer tokens are presented as the JWT string alone, so the seed
/// is never needed again. `expires_in`, when set, bounds the token's lifetime
/// (`exp` claim); the supervisor write token is minted without expiry and simply
/// re-minted on every dispatch, while read tokens are short-lived.
///
/// Pure and I/O-free — unit-testable without a NATS server.
pub fn mint_token(
    config: &LogStreamingConfig,
    job_id: Uuid,
    scope: TokenScope,
    expires_in: Option<Duration>,
) -> Result<String, MintError> {
    // The account seed is both the identity and the signing key, so the issuer
    // of the user JWT is the account's own public key.
    let account =
        KeyPair::from_seed(&config.account_seed).map_err(|e| MintError::Seed(e.to_string()))?;
    let account_pub = account.public_key();

    // Ephemeral subject identity. Discarded after signing — the bearer token
    // carries no nkey challenge, so the seed is never used again.
    let user = KeyPair::new_user();

    let scope_subject = subject_scope(job_id);
    let mut token = Token::new_user(account_pub, user.public_key())
        .name(format!("tml-job-{job_id}-{}", scope.label()))
        .bearer_token(true);

    token = match scope {
        TokenScope::Supervisor => token
            .allow_publish(scope_subject)
            .allow_subscribe(console_input_subject(job_id)),
        TokenScope::ConsoleInput => token.allow_publish(console_input_subject(job_id)),
        TokenScope::Read => {
            // Subscribe on the job's log subjects, plus the job-scoped slice
            // of the JetStream API a client needs to run an ordered consumer
            // against the job's stream: STREAM.INFO (byte/sequence counts for
            // computing a bounded replay start), CONSUMER.CREATE (both the
            // bare legacy form and the named form with consumer-name/filter
            // suffix), and INFO / MSG.NEXT (ordered consumers are pull-based)
            // / DELETE for the created consumer. API replies arrive on the
            // client's inboxes, restricted to the per-job prefix — never the
            // account-wide `_INBOX.>`.
            let api = jetstream_api_prefix(config);
            let stream = stream_name(job_id);
            token
                .allow_subscribe(scope_subject)
                .allow_subscribe(format!("{}.>", inbox_prefix(job_id)))
                .allow_publish(format!("{api}.STREAM.INFO.{stream}"))
                .allow_publish(format!("{api}.CONSUMER.CREATE.{stream}"))
                .allow_publish(format!("{api}.CONSUMER.CREATE.{stream}.>"))
                .allow_publish(format!("{api}.CONSUMER.INFO.{stream}.>"))
                .allow_publish(format!("{api}.CONSUMER.MSG.NEXT.{stream}.>"))
                .allow_publish(format!("{api}.CONSUMER.DELETE.{stream}.>"))
        }
    };

    if let Some(ttl) = expires_in {
        // `exp` is unix seconds; saturating add keeps an absurd TTL from
        // wrapping i64 rather than producing a bogus past timestamp.
        let exp = chrono::Utc::now()
            .timestamp()
            .saturating_add(ttl.as_secs() as i64);
        token = token.expires(exp);
    }

    Ok(token.sign(&account))
}

/// Build the [`LogStreamingDispatch`] handed to a supervisor in
/// `StartJobMessage`: the NATS URL, this job's subject prefix, its
/// console-input subject, and a freshly minted supervisor-scoped write token.
/// The token carries no expiry — it is re-minted on every (re)dispatch rather
/// than persisted.
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

/// Creates the per-job JetStream stream at dispatch.
///
/// Abstracted behind a trait so the dispatch path can be unit-tested without a
/// live NATS server (which cannot run in the sandbox, `AGENTS.md` §2): tests run
/// with log streaming disabled and no provisioner, while production uses
/// [`NatsLogStreamProvisioner`]. Mirrors the [`crate::registry::RegistryClient`]
/// injectable-trait pattern.
#[async_trait]
pub trait LogStreamProvisioner: Send + Sync {
    /// Idempotently create the per-job stream capturing `logs.<job-id>.>` with
    /// no `MaxAge` (it never expires; GC is handled separately, out of scope).
    ///
    /// Must be safe to call repeatedly: reconcile re-dispatches `StartJob`
    /// idempotently, so an already-existing stream is a success, not an error.
    async fn ensure_job_stream(&self, job_id: Uuid) -> Result<(), ProvisionError>;

    /// Idempotently create the per-job stream recording `console-in.<job-id>`
    /// with no `MaxAge`. Called **before** a console-input token is minted, so
    /// capture is in place before any client can publish.
    async fn ensure_console_input_stream(&self, job_id: Uuid) -> Result<(), ProvisionError>;
}

/// The log-streaming components the switchboard wires through to the dispatch
/// path: the (non-secret) config used to mint tokens, and the provisioner used
/// to create streams. Cloneable (the provisioner is shared behind an `Arc`); a
/// single instance is built at startup and shared by all supervisor workers.
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

/// The production [`LogStreamProvisioner`]: creates streams over a JetStream
/// management connection to a real NATS server.
///
/// The management connection authenticates with a **non-bearer** user JWT minted
/// from the account seed and scoped to the JetStream API (`$JS.API.>`, plus its
/// reply inbox). Because the switchboard generates that management user's nkey,
/// it can answer the server's nonce challenge — unlike the per-job bearer tokens
/// it hands out, whose seeds are discarded.
pub struct NatsLogStreamProvisioner {
    jetstream: async_nats::jetstream::Context,
}

impl NatsLogStreamProvisioner {
    /// Connect to NATS and build a provisioner with JetStream management rights.
    pub async fn connect(config: &LogStreamingConfig) -> Result<Self, ConnectError> {
        let account = KeyPair::from_seed(&config.account_seed)
            .map_err(|e| ConnectError::Seed(e.to_string()))?;
        let account_pub = account.public_key();

        // A management user the switchboard itself holds the seed for, so it can
        // sign the server's connection nonce. Scoped only to the JetStream API
        // and its reply inbox — enough to create/inspect streams, nothing else.
        let mgmt_user = Arc::new(KeyPair::new_user());
        let mgmt_jwt = Token::new_user(account_pub, mgmt_user.public_key())
            .name("tml-switchboard-logstream-mgmt")
            .allow_publish("$JS.API.>")
            .allow_subscribe("_INBOX.>")
            .sign(&account);

        let signer = mgmt_user.clone();
        let client = async_nats::ConnectOptions::with_jwt(mgmt_jwt, move |nonce| {
            let signer = signer.clone();
            async move { signer.sign(&nonce).map_err(async_nats::AuthError::new) }
        })
        .name("tml-switchboard")
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
}

impl NatsLogStreamProvisioner {
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
            // No MaxAge: the stream never expires (GC is handled separately,
            // out of scope). A zero `Duration` is JetStream's "unlimited age".
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

    /// A throwaway account seed for tests. Generating a fresh account keypair
    /// each run keeps a real secret out of the source tree.
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
    ///
    /// Deliberately parsed as a [`serde_json::Value`] rather than `nats_jwt`'s
    /// own `Claims`: that type's permission lists are emitted with
    /// `skip_serializing_if` but lack a serde default, so it cannot round-trip
    /// its own output. Inspecting the JSON also asserts the exact wire shape the
    /// NATS server will see.
    fn decode_claims(jwt: &str) -> Value {
        let payload = jwt.split('.').nth(1).expect("jwt has a payload segment");
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .expect("payload is base64url");
        serde_json::from_slice(&bytes).expect("payload is JSON")
    }

    /// The `allow` list under a permission direction (`"pub"` or `"sub"`) of a
    /// user JWT's NATS claims, as a `Vec<String>` (empty if the direction is
    /// absent).
    fn allow(claims: &Value, direction: &str) -> Vec<String> {
        claims["nats"][direction]["allow"]
            .as_array()
            .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
            .unwrap_or_default()
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
        // Stream names forbid '.', so the prefix is hyphenated.
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
    }

    #[test]
    fn write_token_is_bearer_and_scoped_to_the_job() {
        let config = test_config();
        let job_id = Uuid::new_v4();

        let jwt = mint_token(&config, job_id, TokenScope::Supervisor, None).expect("mint");
        let claims = decode_claims(&jwt);

        // Issued by the account's own public key (seed doubles as signing key),
        // which is also the issuer_account (no separate signing key).
        let account_pub = KeyPair::from_seed(&config.account_seed)
            .unwrap()
            .public_key();
        assert_eq!(claims["iss"], account_pub);
        assert_eq!(claims["nats"]["type"], "user");
        assert_eq!(claims["nats"]["issuer_account"], account_pub);
        assert_eq!(claims["nats"]["bearer_token"], true);

        // Publish is scoped to exactly this job's log subjects; subscribe to
        // exactly its console-input subject.
        assert_eq!(allow(&claims, "pub"), vec![subject_scope(job_id)]);
        assert_eq!(allow(&claims, "sub"), vec![console_input_subject(job_id)]);

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

        // Publish to exactly the job's console-input subject — no log
        // subjects, no JetStream API (the server records publishes itself via
        // the subject-bound stream) — and no subscribe scope at all.
        assert_eq!(allow(&claims, "pub"), vec![console_input_subject(job_id)]);
        assert!(allow(&claims, "sub").is_empty());

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
        // never the account-wide `_INBOX.>` (other users' API replies carry
        // other jobs' log data).
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
        let claims = decode_claims(&dispatch.write_token);
        assert_eq!(allow(&claims, "pub"), vec![subject_scope(job_id)]);
        assert_eq!(allow(&claims, "sub"), vec![console_input_subject(job_id)]);
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

    // ---- Live NATS stream creation (hermetic Nix check) ------------------
    //
    // Backfills the Phase 2 deliverable left unwritten because the sandbox
    // can't run a broker: assert `ensure_job_stream` actually creates the
    // per-job JetStream stream. Gated on `TML_TEST_NATS_SERVER` (the
    // nats-server binary), set only by the `nats-log-streaming` Nix check;
    // unset (plain `nextest` / the sandbox) → the test skips.

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

    /// A child `nats-server` with JetStream enabled (no auth); killed on drop,
    /// with its JetStream store directory removed.
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

        /// Like [`NatsServer::start`], but from a rendered config file
        /// (`render(port, store_dir)`), for servers with auth. Readiness is
        /// polled with `ready_connect` since anonymous connects are rejected
        /// once accounts are configured.
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

    #[tokio::test]
    async fn nats_live_provisioner_creates_job_stream() {
        let Some(bin) = nats_server_bin() else {
            eprintln!("TML_TEST_NATS_SERVER unset; skipping live NATS provisioner test");
            return;
        };
        let server = NatsServer::start(&bin).await;

        // No-auth connection: this asserts the provisioning behavior, not the
        // (separately unit-tested) bearer-JWT auth model.
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

        // The stream exists, capturing exactly this job's subjects, never
        // expiring (max_age == 0).
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

    /// The read token's granted subject set must be *sufficient* for the web
    /// console's bounded replay-then-follow: STREAM.INFO for the start-seq
    /// computation, an ordered consumer over the job's stream, and the live
    /// tail — all through inboxes under the per-job prefix.
    ///
    /// The server enforces the token's **decoded allow lists** on a
    /// config-file `reader` user: password auth stands in for the bearer-JWT
    /// transport (a full operator/account hierarchy is beyond `nats-jwt`),
    /// while the permission set under test is exactly the minted one. The JS
    /// browser client drives the same `$JS.API` subjects as async-nats here.
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
        let quote = |subjects: Vec<String>| {
            subjects
                .iter()
                .map(|s| format!("\"{s}\""))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let pub_allow = quote(allow(&claims, "pub"));
        let sub_allow = quote(allow(&claims, "sub"));

        let mgmt_opts = || {
            async_nats::ConnectOptions::new()
                .user_and_password("mgmt".to_string(), "mgmt".to_string())
        };
        let server = NatsServer::start_with_config(
            &bin,
            |port, store| {
                format!(
                    r#"
listen: "127.0.0.1:{port}"
jetstream {{ store_dir: "{store}" }}
accounts {{
  TEST {{
    jetstream: enabled
    users: [
      {{ user: "mgmt", password: "mgmt" }},
      {{ user: "reader", password: "reader", permissions: {{
        publish: {{ allow: [{pub_allow}] }},
        subscribe: {{ allow: [{sub_allow}] }},
      }} }}
    ]
  }}
}}
"#
                )
            },
            mgmt_opts(),
        )
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
        let reader = async_nats::ConnectOptions::new()
            .user_and_password("reader".to_string(), "reader".to_string())
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

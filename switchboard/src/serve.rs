use crate::config::{DatabaseConfig, DatabaseCredentials, SwitchboardConfig};
use crate::events::EventBus;
use crate::log_streaming::{LogStreaming, NatsLogStreamProvisioner};
use crate::registry::{OciRegistryClient, RegistryClient};
use anyhow::Context;
use sqlx::PgPool;
use sqlx::postgres::PgConnectOptions;
use std::net::SocketAddr;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;

pub struct AppStateInner {
    pg_pool: PgPool,
    config: SwitchboardConfig,
    /// Pulls manifests/indexes by digest for image-catalog registration.
    /// Injectable so route tests can use a canned in-memory registry.
    registry: Arc<dyn RegistryClient>,
    /// Log-streaming components (token minting + stream provisioning), present
    /// only when the deployment enables log streaming. `None` disables the
    /// feature: jobs dispatch without a streaming destination. Built once at
    /// startup and shared with every supervisor worker.
    log_streaming: Option<LogStreaming>,
    /// The per-process fan-out for `tml_events` DB change notifications. In
    /// production [`serve`] spawns its listener; test-constructed states carry
    /// a bus nothing feeds, so consumers fall back to their timers.
    event_bus: EventBus,
}

impl AppStateInner {
    pub fn pool(&self) -> &PgPool {
        &self.pg_pool
    }
    pub fn config(&self) -> &SwitchboardConfig {
        &self.config
    }
    pub fn registry(&self) -> &Arc<dyn RegistryClient> {
        &self.registry
    }
    pub fn log_streaming(&self) -> Option<&LogStreaming> {
        self.log_streaming.as_ref()
    }
    pub fn event_bus(&self) -> &EventBus {
        &self.event_bus
    }
}

#[derive(Clone)]
pub struct AppState(Arc<AppStateInner>);
impl AppState {
    pub fn new(pg_pool: PgPool, config: SwitchboardConfig) -> Self {
        Self::with_components(
            pg_pool,
            config,
            Arc::new(OciRegistryClient::new()),
            None,
            EventBus::default(),
        )
    }

    /// Construct an [`AppState`] with an explicit registry client. Used by
    /// catalog route tests to inject a canned in-memory registry. Log streaming
    /// is disabled (tests have no NATS).
    pub fn with_registry(
        pg_pool: PgPool,
        config: SwitchboardConfig,
        registry: Arc<dyn RegistryClient>,
    ) -> Self {
        Self::with_components(pg_pool, config, registry, None, EventBus::default())
    }

    /// Construct an [`AppState`] from all of its injectable components. The
    /// production entry point ([`serve`]) uses this to attach the log-streaming
    /// provisioner and the fed event bus it builds at startup.
    pub fn with_components(
        pg_pool: PgPool,
        config: SwitchboardConfig,
        registry: Arc<dyn RegistryClient>,
        log_streaming: Option<LogStreaming>,
        event_bus: EventBus,
    ) -> Self {
        AppState(Arc::new(AppStateInner {
            pg_pool,
            config,
            registry,
            log_streaming,
            event_bus,
        }))
    }
}
impl Deref for AppState {
    type Target = AppStateInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(clap::Args, Debug)]
pub struct ServeCommand {
    #[arg(short = 'c', long = "config", env = "TML_CFG_FILE")]
    config: Option<PathBuf>,
}

pub async fn pg_pool_from_config(db_config: &DatabaseConfig) -> Result<PgPool, sqlx::Error> {
    // TODO: .ssl_mode(PgSslMode::VerifyFull)
    let pg_options = PgConnectOptions::new()
        .host(&db_config.host)
        .database(&db_config.database)
        .username(&db_config.user);
    let pg_options = match db_config.port {
        None => pg_options,
        Some(port) => pg_options.port(port),
    };
    let pg_options = match &db_config.auth {
        None => pg_options,
        Some(DatabaseCredentials::Password(password)) => pg_options.password(password),
    };
    PgPool::connect_with(pg_options).await
}

pub async fn serve(serve_command: ServeCommand) -> anyhow::Result<()> {
    let config = super::config::load_configuration(serve_command.config.as_deref())?;

    tracing_subscriber::fmt::init();

    // The mock OAuth provider is an unauthenticated login bypass intended only
    // for local development; warn loudly at startup if it is enabled so it can
    // never run in production unnoticed.
    if config
        .oauth
        .mock
        .as_ref()
        .map(|m| m.enabled)
        .unwrap_or(false)
    {
        tracing::warn!(
            "-- WARNING -- DEVELOPMENT-ONLY MOCK OAUTH PROVIDER IS ENABLED. \
             It mints valid sessions for built-in identities with NO authentication. \
             PLEASE DO NOT USE THIS IN PRODUCTION."
        );
    }

    let pg_pool = pg_pool_from_config(&config.database)
        .await
        .context("failed to connect to database")?;

    // Apply database migrations automatically. The migrations are embedded in
    // this binary, and any changes to ./migrations (from the project root) will
    // be picked up by the build.rs script:
    sqlx::migrate!()
        .run(&pg_pool)
        .await
        .context("failed to migrate database")?;

    let bind_address = config.server.bind_address;

    // The process-wide change-notification fan-out and its single LISTEN
    // connection (see `crate::events`).
    let event_bus = EventBus::default();
    tokio::spawn(event_bus.listener(pg_pool.clone()));

    // Spawn the job scheduler. It coordinates with the per-host supervisor
    // workers entirely through the database (no in-process channel), so it just
    // needs its own pool handle; see `crate::scheduler`.
    let scheduler = crate::scheduler::Scheduler::new(
        pg_pool.clone(),
        config.service.match_interval,
        config.service.host_liveness_timeout,
        &event_bus,
        config.service.scheduler_event_debounce,
    );
    tokio::spawn(scheduler.run());

    // Establish the log-streaming management connection up front (if enabled),
    // so a misconfigured NATS endpoint fails fast at startup rather than on the
    // first dispatch. The provisioner is shared by every supervisor worker.
    let log_streaming = match &config.log_streaming {
        Some(ls_config) => {
            tracing::info!(
                nats_url = %ls_config.nats_url,
                "log streaming enabled; connecting to NATS for stream provisioning"
            );
            let provisioner = NatsLogStreamProvisioner::connect(ls_config)
                .await
                .context("failed to connect to NATS for log streaming")?;
            Some(LogStreaming {
                config: ls_config.clone(),
                provisioner: Arc::new(provisioner),
            })
        }
        None => None,
    };

    let app_state = AppState::with_components(
        pg_pool,
        config,
        Arc::new(OciRegistryClient::new()),
        log_streaming,
        event_bus,
    );
    let router = super::routes::build_router(app_state);

    let listener = tokio::net::TcpListener::bind(bind_address)
        .await
        .with_context(|| format!("failed to bind server to {bind_address}"))?;
    tracing::info!("Bound server to {bind_address}");

    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("(server exited)")
}

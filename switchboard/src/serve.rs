use crate::config::{DatabaseConfig, DatabaseCredentials, SwitchboardConfig};
use crate::registry::{OciRegistryClient, RegistryClient};
use miette::{IntoDiagnostic, WrapErr};
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
}

#[derive(Clone)]
pub struct AppState(Arc<AppStateInner>);
impl AppState {
    pub fn new(pg_pool: PgPool, config: SwitchboardConfig) -> Self {
        Self::with_registry(pg_pool, config, Arc::new(OciRegistryClient::new()))
    }

    /// Construct an [`AppState`] with an explicit registry client. Used by
    /// catalog route tests to inject a canned in-memory registry.
    pub fn with_registry(
        pg_pool: PgPool,
        config: SwitchboardConfig,
        registry: Arc<dyn RegistryClient>,
    ) -> Self {
        AppState(Arc::new(AppStateInner {
            pg_pool,
            config,
            registry,
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

pub async fn serve(serve_command: ServeCommand) -> miette::Result<()> {
    let config = super::config::load_configuration(serve_command.config.as_deref())?;

    if config.log.use_tokio_console_subscriber {
        console_subscriber::init();
    } else {
        // Note: this is DIFFERENT from `tracing_subscriber::fmt().init()`
        tracing_subscriber::fmt::init();
    }

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
        .into_diagnostic()
        .wrap_err("failed to connect to database")?;

    // Apply database migrations automatically. The migrations are embedded in
    // this binary, and any changes to ./migrations (from the project root) will
    // be picked up by the build.rs script:
    sqlx::migrate!()
        .run(&pg_pool)
        .await
        .into_diagnostic()
        .wrap_err("failed to migrate database")?;

    let bind_address = config.server.bind_address;
    let tls_config = config.server.testing_only_tls_config.clone();

    // Spawn the job scheduler. It coordinates with the per-host supervisor
    // workers entirely through the database (no in-process channel), so it just
    // needs its own pool handle; see `crate::scheduler`.
    let scheduler = crate::scheduler::Scheduler::new(
        pg_pool.clone(),
        config.service.match_interval,
        config.service.host_liveness_timeout,
    );
    tokio::spawn(scheduler.run());

    let app_state = AppState::new(pg_pool, config);
    let router = super::routes::build_router(app_state);

    enum Server {
        PlainHttp(axum_server::Server<SocketAddr>),
        Tls(axum_server::Server<SocketAddr, axum_server::tls_rustls::RustlsAcceptor>),
    }

    let server = match tls_config {
        None => Server::PlainHttp(axum_server::bind(bind_address)),
        Some(tls) => {
            let rustls_config =
                axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.cert, &tls.key)
                    .await
                    .into_diagnostic()
                    .wrap_err("Failed to load RusTls configuration for public server")?;
            let server = axum_server::bind_rustls(bind_address, rustls_config);

            tracing::warn!(
                "-- WARNING -- DEVELOPMENT-ONLY TLS MODE IS ENABLED. PLEASE DO NOT USE THIS IN PRODUCTION."
            );

            Server::Tls(server)
        }
    };
    tracing::info!("Bound server to {bind_address}");

    match server {
        Server::PlainHttp(server) => {
            server
                .serve(router.into_make_service_with_connect_info::<SocketAddr>())
                .await
        }
        Server::Tls(server) => {
            server
                .serve(router.into_make_service_with_connect_info::<SocketAddr>())
                .await
        }
    }
    .into_diagnostic()
    .wrap_err("(server exited)")
}

//! The switchboard server.

use crate::api;
use crate::cfg::{Config, DatabaseAuth, PasswordAuth};
use crate::supervisor::{socket, Herd, JobMarket};
use axum::extract::FromRef;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::Router;
use axum_extra::extract::cookie::Key;
// use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use axum_server::{accept::DefaultAcceptor, Server};
use http::StatusCode;
use miette::{IntoDiagnostic, WrapErr};
use sqlx::{postgres::PgConnectOptions, PgPool};
use std::net::SocketAddr;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use tracing::instrument;

pub mod auth;
pub mod session;
pub mod token;

/// State of the server. Only one of these objects ever exists at a time.
/// This type must also be [`Sync`].
#[derive(Debug)]
pub struct AppStateInner {
    /// Connection pool to the database server.
    db_pool: PgPool,

    /// Supervisor herd
    herd: Arc<Herd>,

    job_market: Arc<JobMarket>,

    /// Server configuration, set at startup
    config: Config,
    /// Used for cookie signing; should not be touched
    cookie_signing_key: Key,
}
impl AppStateInner {
    pub fn pool(&self) -> &PgPool {
        &self.db_pool
    }
    pub fn herd(&self) -> Arc<Herd> {
        self.herd.clone()
    }
    pub fn job_market(&self) -> Arc<JobMarket> {
        self.job_market.clone()
    }
    pub fn config(&self) -> &Config {
        &self.config
    }
}
/// Thin wrapper around an [`Arc`] of an [`AppStateInner`], since [`State`](axum::extract::State)
/// requires [`Clone`].
///
/// Implements `FromRef<Key>` for the [`SignedCookieJar`](axum_extra::extract::cookie::SignedCookieJar)
/// extractor.
#[derive(Debug, Clone)]
pub struct AppState(Arc<AppStateInner>);
impl Deref for AppState {
    type Target = AppStateInner;

    fn deref(&self) -> &Self::Target {
        self.0.deref()
    }
}
impl FromRef<AppState> for Key {
    fn from_ref(input: &AppState) -> Self {
        input.cookie_signing_key.clone()
    }
}

#[derive(clap::Args, Debug)]
pub struct ServeCommand {
    /// Path to server configuration file
    #[arg(short = 'c', long = "cfg", env = "TML_CFG")]
    cfg: PathBuf,
}

/// Main server entry point; starts public and internal servers according to the configuration at
/// `config_path`.
#[instrument]
pub async fn serve(cmd: ServeCommand) -> miette::Result<()> {
    let config_path = &cmd.cfg;

    // Tracing init

    tracing_subscriber::fmt::init();

    // Load configuration

    // TODO: overlayed configuration

    let cfg_text = std::fs::read_to_string(config_path)
        .into_diagnostic()
        .wrap_err("Failed to open configuration file")?;
    let cfg: Config = toml::from_str(&cfg_text)
        .into_diagnostic()
        .wrap_err_with(|| {
            format!(
                "Failed to parse configuration file {}",
                config_path.display()
            )
        })?;

    tracing::info!("Serving with configuration: {cfg:?}");

    // Connect to database

    let pg_options = PgConnectOptions::new()
        .host(&cfg.database.address)
    // .port(cfg.database.port)
        .database(&cfg.database.name)
    /*
        .ssl_mode(PgSslMode::VerifyFull)
        /* TODO: supply ssl client cert */
     */
        ;
    let pg_options = match cfg.database.auth {
        DatabaseAuth::PasswordAuth(PasswordAuth {
            ref username,
            ref password,
        }) => pg_options.username(username).password(password),
    };
    let pg_pool = PgPool::connect_with(pg_options)
        .await
        .into_diagnostic()
        .wrap_err("Failed to connect to database")?;

    tracing::info!("Connected to database, poolsz_idle={}", pg_pool.num_idle());

    // Bind TCP listeners.

    let public_socket_addr = cfg.public_server.socket_addr;

    // let rustls_config =
    //     RustlsConfig::from_pem_file(&cfg.public_server.cert, &cfg.public_server.key)
    //         .await
    //         .into_diagnostic()
    //         .wrap_err("Failed to load RusTls configuration for public server")?;
    let public_server_listener = axum_server::bind(public_socket_addr); //, rustls_config);

    tracing::info!("Bound TCP listener on: (public) {public_socket_addr}");

    // Build shared state

    let state_inner = AppStateInner {
        db_pool: pg_pool,
        herd: Arc::new(Herd::new()),
        job_market: Arc::new(JobMarket::new()),
        config: cfg,
        cookie_signing_key: Key::generate(),
    };

    let server_state = AppState(Arc::new(state_inner));

    // Spawn server tasks

    match tokio::spawn(
        async move { serve_public_server(public_server_listener, server_state).await },
    )
    .await
    {
        Ok(()) => {
            tracing::info!("Server exited successfully");
        }
        Err(e) => {
            tracing::error!("Server exited with error: {e}");
        }
    }

    Ok(())
}

/// Serve the public server.
///
/// Should be run in its own `tokio` task.
async fn serve_public_server(server: Server<DefaultAcceptor>, state: AppState) {
    // fallback when the requested path doesn't exist
    async fn not_found() -> impl IntoResponse {
        StatusCode::NOT_FOUND
    }

    let router = Router::new()
        // Session management
        .nest("/session", session_router())
        // API endpoints
        .nest("/api/v1", api_router())
        // supervisor websocket endpoint
        .route("/supervisor", get(socket::supervisor_handler))
        // miscellanea
        .fallback(not_found)
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    tracing::info!("Starting public server");

    match server
        .serve(router.into_make_service_with_connect_info::<SocketAddr>())
        .await
    {
        Ok(()) => tracing::info!("Public server exited successfully"),
        Err(e) => tracing::error!("Public server exited with error: {e:?}"),
    }
}

/// Sub-router for API endpoints.
pub fn api_router() -> Router<AppState> {
    Router::new()
        // job management group
        .route(
            "/job/queue",
            post(api::jobs::enqueue).get(api::jobs::get_queue),
        )
        .route("/job/:id", delete(api::jobs::cancel))
        .route("/job/:id/info", get(api::jobs::info))
        .route("/job/:id/status", get(api::jobs::status))
        .route("/supervisor/list", get(api::supervisors::list))
        .route("/supervisor/:id/status", get(api::supervisors::status))
    //-- Creating & deleting supervsiors
    // .route("/supervisor/register", post(api::supervisors::register))
    // .route("/supervisor/:id", delete(api::supervisors::unregister))
    //-- Remotely turning off supervisors (?)
    // .route("/supervisor/:id/status", put(api::supervisors::/* todo */))
}

/// Sub-router for session endpoints.
pub fn session_router() -> Router<AppState> {
    Router::new()
        // Standard login endpoint.
        .route("/login", post(session::login_handler))
    // TODO: other session management (logout)
}

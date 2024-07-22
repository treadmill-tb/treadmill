//! The switchboard server.

use crate::perms;
use axum::extract::FromRef;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use axum_extra::extract::cookie::Key;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use axum_server::Server;
use http::StatusCode;
use miette::{IntoDiagnostic, WrapErr};
use sqlx::{postgres::PgConnectOptions, PgPool};
use std::net::SocketAddr;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::task::JoinError;
use tower_http::trace::TraceLayer;
use tracing::instrument;
use treadmill_rs::config::{self, TreadmillConfig};

pub mod auth;
mod public;
mod session;
pub mod socket;
pub mod token;

/// State of the server. Only one of these objects ever exists at a time.
/// This type must also be [`Sync`].
#[derive(Debug)]
pub struct AppStateInner {
    /// Connection pool to the database server.
    db_pool: PgPool,
    /// Server configuration, set at startup
    config: TreadmillConfig,
    /// Used for cookie signing; should not be touched
    cookie_signing_key: Key,
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

#[derive(clap::Parser, Debug)]
pub struct ServeCommand {
    #[clap(long, env = "TREADMILL_CONFIG")]
    config: Option<PathBuf>,
}

/// Main server entry point; starts public and internal servers according to the configuration at
/// `config_path`.
#[instrument]
pub async fn serve(cli: ServeCommand) -> miette::Result<()> {
    // Tracing init
    tracing_subscriber::fmt::init();

    // Load configuration
    let config = config::load_config()
        .into_diagnostic()
        .wrap_err("Failed to load configuration")?;

    tracing::info!("Serving with configuration: {:?}", config.switchboard);

    // Connect to database
    let pg_options = PgConnectOptions::new()
        .host(&config.switchboard.database.address)
        .port(config.switchboard.database.port)
        .database(&config.switchboard.database.name);
    //  .ssl_mode(PgSslMode::VerifyFull)
    /* TODO: supply ssl client cert */

    let pg_options = match config.switchboard.database.auth {
        config::DatabaseAuth::PasswordAuth(config::PasswordAuth {
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
    let public_socket_addr = config.switchboard.public_server.socket_addr;
    let internal_socket_addr = config.switchboard.internal_server.socket_addr;

    let rustls_config = RustlsConfig::from_pem_file(
        &config.switchboard.public_server.cert,
        &config.switchboard.public_server.key,
    )
    .await
    .into_diagnostic()
    .wrap_err("Failed to load RusTls configuration for public server")?;

    let public_server_listener = axum_server::bind_rustls(public_socket_addr, rustls_config);

    let internal_server_listener = TcpListener::bind(internal_socket_addr)
        .await
        .into_diagnostic()
        .wrap_err_with(|| {
            format!("Failed to bind TcpListener for internal server at {internal_socket_addr}")
        })?;

    tracing::info!("Bound TCP listeners on:\n\t(public) {public_socket_addr}\n\t(internal) {internal_socket_addr}");

    // Build shared state
    let state_inner = AppStateInner {
        db_pool: pg_pool,
        config,
        cookie_signing_key: Key::generate(),
    };

    let (public_server_state, internal_server_state) = {
        let arc = AppState(Arc::new(state_inner));
        (arc.clone(), arc)
    };

    // Spawn server tasks
    let public_server = tokio::spawn(async move {
        serve_public_server(public_server_listener, public_server_state).await
    });
    let internal_server = tokio::spawn(async move {
        serve_internal_server(internal_server_listener, internal_server_state).await
    });

    let (psr, isr): (Result<(), JoinError>, Result<(), JoinError>) =
        tokio::join!(public_server, internal_server);

    tracing::debug!("psr: {psr:?}");
    tracing::debug!("isr: {isr:?}");

    Ok(())
}

/// Serve the public server.
///
/// Should be run in its own `tokio` task.
async fn serve_public_server(server: Server<RustlsAcceptor>, state: AppState) {
    // fallback when the requested path doesn't exist
    async fn not_found() -> impl IntoResponse {
        StatusCode::NOT_FOUND
    }

    let router = Router::new()
        // Session management
        .nest("/session", public::build_session_router())
        // API endpoints
        .nest("/api", public::build_api_router())
        // supervisor websocket endpoint
        .route("/supervisor", get(socket::supervisor_handler))
        // for testing/debugging:
        .route("/perm_test", get(perms::jobs::example))
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

/// Serve the internal control server.
///
/// Should be run in its own `tokio` task.
async fn serve_internal_server(tcp_listener: TcpListener, state: AppState) {
    let router = Router::new().with_state(state);

    tracing::info!("Starting internal server");

    match axum::serve(tcp_listener, router).await {
        Ok(()) => tracing::info!("Internal server exited successfully"),
        Err(e) => tracing::error!("Internal server exited with error: {e:?}"),
    }
}

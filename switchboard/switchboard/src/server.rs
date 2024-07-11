use crate::cfg::{Config, DatabaseAuth, PasswordAuth};
use axum::extract::FromRef;
use axum::routing::get;
use axum::Router;
use axum_extra::extract::cookie::Key;
use miette::{IntoDiagnostic, WrapErr};
use sqlx::{postgres::PgConnectOptions, PgPool};
use std::net::SocketAddr;
use std::ops::Deref;
use std::path::Path;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::task::JoinError;
use tower_http::trace::TraceLayer;
use tracing::instrument;

mod auth;
mod session;
mod socket;
pub mod token;
mod web;

#[derive(Debug)]
pub struct AppStateInner {
    db_pool: PgPool,
    #[allow(dead_code)]
    config: Config,
    cookie_signing_key: Key,
}
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

#[instrument]
pub async fn serve(config_path: &Path) -> miette::Result<()> {
    // Tracing init

    tracing_subscriber::fmt::init();

    // Load configuration

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
        .port(cfg.database.port)
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
    let internal_socket_addr = cfg.internal_server.socket_addr;

    let public_server_listener = TcpListener::bind(public_socket_addr)
        .await
        .into_diagnostic()
        .wrap_err_with(|| {
            format!("Failed to bind TcpListener for public server at {public_socket_addr}")
        })?;
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
        config: cfg,
        cookie_signing_key: axum_extra::extract::cookie::Key::generate(),
    };

    let (public_server_state, internal_server_state) = {
        let arc = AppState(Arc::new(state_inner));
        (arc.clone(), arc)
    };

    // Create servers

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

async fn serve_public_server(tcp_listener: TcpListener, state: AppState) {
    // TODO: TLS

    let router = Router::new()
        .nest("/session", web::build_session_router())
        .nest("/api", web::build_api_router())
        .route("/supervisor", get(socket::supervisor_handler))
        .route("/perm_test", get(auth::example))
        // TODO: web routes
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    tracing::info!("Starting public server");

    match axum::serve(
        tcp_listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    {
        Ok(()) => tracing::info!("Public server exited successfully"),
        Err(e) => tracing::error!("Public server exited with error: {e:?}"),
    }
}

async fn serve_internal_server(tcp_listener: TcpListener, state: AppState) {
    let router = Router::new().with_state(state);

    tracing::info!("Starting internal server");

    match axum::serve(tcp_listener, router).await {
        Ok(()) => tracing::info!("Internal server exited successfully"),
        Err(e) => tracing::error!("Internal server exited with error: {e:?}"),
    }
}

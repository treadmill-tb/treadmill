//! Process entry point: shared application state and the `serve` command.

use std::net::SocketAddr;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;

use miette::{IntoDiagnostic, WrapErr};
use tokio::net::TcpListener;
use treadmill_rs::api::switchboard::client::SwitchboardClient;

use crate::config::ConsoleConfig;

pub struct AppStateInner {
    config: ConsoleConfig,
}

impl AppStateInner {
    pub fn config(&self) -> &ConsoleConfig {
        &self.config
    }

    /// A switchboard client for the configured instance, carrying `token` if
    /// the caller is authenticated (`None` for anonymous calls such as building
    /// the login URL).
    pub fn switchboard(&self, token: Option<String>) -> SwitchboardClient {
        SwitchboardClient::new(self.config.switchboard.base_url.clone(), token)
    }
}

/// Cheap-to-clone handle to the shared, immutable console state.
#[derive(Clone)]
pub struct AppState(Arc<AppStateInner>);

impl AppState {
    pub fn new(config: ConsoleConfig) -> Self {
        AppState(Arc::new(AppStateInner { config }))
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
    #[arg(short = 'c', long = "config", env = "TML_CONSOLE_CFG_FILE")]
    config: Option<PathBuf>,
}

pub async fn serve(serve_command: ServeCommand) -> miette::Result<()> {
    let config = crate::config::load_configuration(serve_command.config.as_deref())?;

    tracing_subscriber::fmt::init();

    let bind_address = config.server.bind_address;
    let state = AppState::new(config);
    let router = crate::routes::build_router(state);

    let listener = TcpListener::bind(bind_address)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to bind console server to {bind_address}"))?;
    tracing::info!("Console listening on {bind_address}");

    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .into_diagnostic()
    .wrap_err("(console server exited)")
}
